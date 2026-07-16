use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Local, TimeZone};
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};

const BRANCH_PARAM_ALIASES: &[&str] = &[
    "codeBranch",
    "branch",
    "gitBranch",
    "sourceBranch",
    "BRANCH",
    "branchName",
];
const IMAGE_PARAM_ALIASES: &[&str] = &[
    "imageVersion",
    "image_version",
    "imageTag",
    "image_tag",
    "IMAGE_VERSION",
    "IMAGE_TAG",
    "tag",
];

#[derive(Clone)]
struct App {
    client: Client,
    config: Value,
    api_prefix: String,
    headers: HeaderMap,
}

#[derive(Clone)]
struct JenkinsSelection {
    server_name: String,
    server: Value,
    env_name: String,
    envs: Value,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{}", red(&format!("{err:#}")));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "version" || arg == "-v") {
        println!("version: 1.3.0\nlatest feature: remove local conflict branch after merge");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "help" || arg == "-h") {
        print_help();
        return Ok(());
    }

    let config = load_config()?;
    let gitlab_url = required_str(&config, "gitlab_url")?.to_string();
    let token = required_str(&config, "gitlab_token")?.to_string();
    let mut headers = HeaderMap::new();
    headers.insert("PRIVATE-TOKEN", HeaderValue::from_str(&token)?);
    let app = App {
        client: Client::builder().cookie_store(true).build()?,
        config,
        api_prefix: format!("{}/api/v4", gitlab_url.trim_end_matches('/')),
        headers,
    };

    let commands = [
        "create", "list", "merge", "projects", "add", "remove", "set", "get", "change",
        "release", "build",
    ];
    if args.is_empty() || !commands.contains(&args[0].as_str()) {
        println!("Available commands:\n");
        for (index, command) in commands.iter().enumerate() {
            println!("{index}.{command}");
        }
        return Ok(());
    }

    match args[0].as_str() {
        "list" => app.merge_list(args.get(1).is_some_and(|v| v == "-A"))?,
        "projects" => app.get_all_project()?,
        "add" => app.add_project()?,
        "remove" => app.remove_project()?,
        "set" => app.set_config(&args[1..])?,
        "get" => app.get_config(&args[1..])?,
        "change" => app.change_version(args.get(1).map(String::as_str))?,
        "release" => app.merge_version_to_release()?,
        "build" => app.handle_build(&args[1..])?,
        "merge" => app.handle_merge(&args[1..])?,
        "create" => app.handle_create(&args[1..])?,
        _ => {}
    }
    Ok(())
}

impl App {
    fn get_mr_url(&self, project_id: i64) -> String {
        format!("{}/projects/{project_id}/merge_requests", self.api_prefix)
    }

    fn request_json(&self, response: Response) -> Result<Value> {
        let status = response.status();
        let text = response.text()?;
        if !status.is_success() {
            bail!("HTTP {status}: {text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn add_project(&self) -> Result<()> {
        let project_name = current_project_name()?;
        let project_url = project_repo_url()?;
        println!("Current project_name: {project_name}, git url : {project_url}\n");

        let mut config = self.config.clone();
        let projects = ensure_array_mut(&mut config, "projects")?;
        if projects
            .iter()
            .any(|p| p.get("url").and_then(Value::as_str) == Some(project_url.as_str()))
        {
            println!("project is exist ");
            return Ok(());
        }

        let project_id = self.get_project_id_by_name(Some(&project_url), &project_name)?;
        println!("add project_id: {project_id}");
        projects.push(json!({
            "project_name": project_name,
            "url": project_url,
            "project_id": project_id,
        }));
        save_config(&config)
    }

    fn remove_project(&self) -> Result<()> {
        let project_name = current_project_name()?;
        let project_url = project_repo_url()?;
        println!("Current project_name: {project_name}, git url : {project_url}\n");

        let mut config = self.config.clone();
        let projects = ensure_array_mut(&mut config, "projects")?;
        projects.retain(|p| {
            let keep = p.get("url").and_then(Value::as_str) != Some(project_url.as_str());
            if !keep {
                println!("remote project {}", p.get("url").and_then(Value::as_str).unwrap_or(""));
            }
            keep
        });
        save_config(&config)
    }

    fn get_all_project(&self) -> Result<()> {
        for p in config_array(&self.config, "projects") {
            println!(
                "project_name: {} , project_id: {} , repo: {}",
                red(str_field(p, "project_name")),
                p.get("project_id").unwrap_or(&Value::Null),
                str_field(p, "url")
            );
        }
        Ok(())
    }

    fn project_in_namespace(&self, project: &Value) -> bool {
        let Some(namespaces) = self.config.get("gitlab_namespace").and_then(Value::as_array) else {
            return true;
        };
        let name = str_field(project, "name_with_namespace");
        namespaces
            .iter()
            .filter_map(Value::as_str)
            .any(|namespace| name.contains(namespace))
    }

    fn get_project_id(&self, project_name: &str) -> Result<i64> {
        let project_url = project_repo_url()?;
        println!("Current git url : {project_url}\n");
        for p in config_array(&self.config, "projects") {
            if p.get("url").and_then(Value::as_str) == Some(project_url.as_str()) {
                return number_field(p, "project_id");
            }
        }
        self.get_project_id_by_name(Some(&project_url), project_name)
    }

    fn get_project_id_by_name(&self, repo_url: Option<&str>, project_name: &str) -> Result<i64> {
        println!("Getting project_id...\n");
        let visibility = self
            .config
            .get("visibility")
            .and_then(Value::as_str)
            .unwrap_or("private");
        let url = format!("{}/projects", self.api_prefix);
        let response = self
            .client
            .get(url)
            .headers(self.headers.clone())
            .query(&[("visibility", visibility), ("search", project_name)])
            .send()?;
        let projects = self.request_json(response)?;
        let mut matches = Vec::new();
        for project in projects.as_array().unwrap_or(&Vec::new()) {
            let path_match = project.get("path").and_then(Value::as_str) == Some(project_name);
            let url_match = repo_url.is_none()
                || project.get("ssh_url_to_repo").and_then(Value::as_str) == repo_url;
            if path_match && self.project_in_namespace(project) && url_match {
                matches.push(project.clone());
            }
        }
        if matches.is_empty() {
            println!("{}", red("No project match! please check your settings or authorization\n"));
            println!("Searched projects:\n");
            for project in projects.as_array().unwrap_or(&Vec::new()) {
                println!("{}", str_field(project, "path"));
            }
            bail!("no project match");
        }
        if matches.len() == 1 {
            return number_field(&matches[0], "id");
        }
        let choices = matches
            .iter()
            .map(|p| str_field(p, "http_url_to_repo").to_string())
            .collect::<Vec<_>>();
        let selected = choose("Choose project", &choices)?;
        number_field(&matches[selected], "id")
    }

    fn create_merge_request(
        &self,
        project_id: i64,
        source_branch: &str,
        target_branch: &str,
        remove_source_branch: bool,
        merge_immediately: bool,
    ) -> Result<Option<String>> {
        let response = self
            .client
            .post(self.get_mr_url(project_id))
            .headers(self.headers.clone())
            .form(&[
                ("source_branch", source_branch),
                ("target_branch", target_branch),
                ("title", source_branch),
                ("description", "created by mr.py"),
                (
                    "remove_source_branch",
                    if remove_source_branch { "true" } else { "false" },
                ),
            ])
            .send()?;
        println!("{response:?}");
        let res = self.request_json(response)?;
        if let Some(web_url) = res.get("web_url").and_then(Value::as_str) {
            if res.get("changes_count").is_some_and(Value::is_null) {
                println!("{source_branch} -> {target_branch}: will be ignored, because there is no change\n");
                println!("Maybe you forget to push your commits or choose wrong branch\n");
                if let Some(iid) = res.get("iid").and_then(Value::as_i64) {
                    let close_url = format!("{}/projects/{project_id}/merge_requests/{iid}", self.api_prefix);
                    let _ = self
                        .client
                        .put(close_url)
                        .headers(self.headers.clone())
                        .form(&[("state_event", "close")])
                        .send();
                }
                return Ok(None);
            }
            println!("{source_branch} -> {target_branch}: {web_url}");
            if merge_immediately {
                confirm_yes_or_exit("would you want to merge immediately\n")?;
                self.merge_gitlab_mr(web_url)?;
            }
            return Ok(Some(web_url.to_string()));
        }
        println!("{res}");
        Ok(res
            .get("message")
            .and_then(Value::as_str)
            .map(ToString::to_string))
    }

    fn create_branch_request(&self, project_name: &str, branch_name: &str, reference: &str) -> Result<()> {
        println!("Creating current branch...");
        let project_id = self.get_project_id(project_name)?;
        let url = format!(
            "{}/projects/{project_id}/repository/branches?branch={}&ref={}",
            self.api_prefix,
            urlencoding::encode(branch_name),
            urlencoding::encode(reference)
        );
        println!("{url}");
        let response = self.client.post(url).headers(self.headers.clone()).send()?;
        println!("{response:?}");
        Ok(())
    }

    fn create_tag_request(
        &self,
        project_name: &str,
        tag_name: &str,
        reference: &str,
        message: &str,
    ) -> Result<()> {
        println!("Creating current branch...");
        let project_id = self.get_project_id(project_name)?;
        let url = format!(
            "{}/projects/{project_id}/repository/tags?branch={}&ref={}&message={}",
            self.api_prefix,
            urlencoding::encode(tag_name),
            urlencoding::encode(reference),
            urlencoding::encode(message)
        );
        println!("{url}");
        let response = self.client.post(url).headers(self.headers.clone()).send()?;
        println!("{response:?}");
        Ok(())
    }

    fn create_merge_request_from_conflict_branch(
        &self,
        current_branch: &str,
        project_name: &str,
    ) -> Result<Option<String>> {
        println!("Pushing current branch...");
        run_git(&["push", "origin", current_branch])?;
        println!("Creating merge request...\n");
        let target_branch = get_target_branch(current_branch)?;
        let project_id = self.get_project_id(project_name)?;
        self.create_merge_request(project_id, current_branch, &target_branch, true, false)
    }

    fn merge_gitlab_mr(&self, web_url: &str) -> Result<()> {
        let (_, mr_iid, project_name) = parse_mr_url(web_url)?;
        let project_id = self.get_project_id(&project_name)?;
        let mr_api_url = format!("{}/projects/{project_id}/merge_requests/{mr_iid}", self.api_prefix);
        let mr = self.request_json(
            self.client
                .get(&mr_api_url)
                .headers(self.headers.clone())
                .send()?,
        )?;
        let source = str_field(&mr, "source_branch");
        let target = str_field(&mr, "target_branch");
        confirm_yes_or_exit(&format!("Confirm to merge {source} into {target}? (Y/N)\n"))?;
        if mr.get("has_conflicts").and_then(Value::as_bool).unwrap_or(false) {
            println!("{}", red(&format!("Merge request {mr_iid} has conflicts and cannot be merged \n")));
            confirm_yes_or_exit("Do you want to checkout a new branch to resolve conflicts? (Y/N)\n")?;
            if run_git(&["checkout", "--track", &format!("origin/{target}")]).is_err() {
                run_git(&["checkout", target])?;
            }
            println!("Pulling from origin to branch {target}\n");
            run_git(&["pull", "origin", target])?;
            println!("Switching to conflict resolving branch\n");
            let conflict_branch = format!("conflict/{source}--conflict-to--{target}");
            run_git(&["checkout", "-b", &conflict_branch])?;
            println!("Now to resolve conflicts, you just need to open your IDE or source control tool to\n`merge {source} into {conflict_branch}`");
        } else {
            self.client
                .put(format!("{mr_api_url}/merge"))
                .headers(self.headers.clone())
                .send()?
                .error_for_status()?;
            println!("Merge request {mr_iid} merged successfully!");
        }
        Ok(())
    }

    fn merge_list(&self, all_scope: bool) -> Result<()> {
        let scope = if all_scope { "&scope=all" } else { "" };
        for p in config_array(&self.config, "projects") {
            let project_id = number_field(p, "project_id")?;
            let url = format!(
                "{}/projects/{project_id}/merge_requests?state=opened{scope}",
                self.api_prefix
            );
            let mrs = self.request_json(self.client.get(url).headers(self.headers.clone()).send()?)?;
            for (index, mr) in mrs.as_array().unwrap_or(&Vec::new()).iter().enumerate() {
                let status = if mr.get("has_conflicts").and_then(Value::as_bool).unwrap_or(false) {
                    red("[conflict]")
                } else {
                    green("[ok]")
                };
                println!(
                    "{}.{} -> {}: {} {}\n",
                    index + 1,
                    str_field(mr, "source_branch"),
                    str_field(mr, "target_branch"),
                    str_field(mr, "web_url"),
                    status
                );
            }
        }
        Ok(())
    }

    fn change_version(&self, arg_version: Option<&str>) -> Result<()> {
        let version = arg_version
            .or_else(|| self.config.pointer("/codebases/version").and_then(Value::as_str))
            .ok_or_else(|| anyhow!("version is missing"))?;
        for p in config_array(&self.config, "projects") {
            let project_id = number_field(p, "project_id")?;
            let url = format!(
                "{}/projects/{project_id}/repository/branches/{}",
                self.api_prefix,
                urlencoding::encode(version)
            );
            let response = self.client.get(url).headers(self.headers.clone()).send()?;
            if response.status().is_success() {
                let branch = response.json::<Value>()?;
                let commit = branch.get("commit").unwrap_or(&Value::Null);
                println!(
                    "project: {} version: {} last_committed_date {} committer_name {} title {} message {}",
                    red(str_field(p, "project_name")),
                    version,
                    str_field(commit, "committed_date"),
                    str_field(commit, "committer_name"),
                    str_field(commit, "title"),
                    str_field(commit, "message")
                );
            }
        }
        Ok(())
    }

    fn merge_version_to_release(&self) -> Result<()> {
        let version = self
            .config
            .pointer("/codebases/version")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("codebases.version is missing"))?;
        let release = self
            .config
            .get("release")
            .and_then(Value::as_str)
            .unwrap_or("uat");
        for p in config_array(&self.config, "projects") {
            let project_id = number_field(p, "project_id")?;
            let url = format!(
                "{}/projects/{project_id}/repository/branches/{}",
                self.api_prefix,
                urlencoding::encode(version)
            );
            if self.client.get(url).headers(self.headers.clone()).send()?.status().is_success() {
                self.create_merge_request(project_id, version, release, false, false)?;
            }
        }
        Ok(())
    }

    fn set_config(&self, args: &[String]) -> Result<()> {
        if args.len() < 2 {
            bail!("usage: mr set <key> <value>");
        }
        let mut config = self.config.clone();
        if args[0] == "version" {
            config["codebases"]["version"] = Value::String(args[1].clone());
        }
        config[&args[0]] = Value::String(args[1].clone());
        save_config(&config)
    }

    fn get_config(&self, args: &[String]) -> Result<()> {
        let key = args.first().ok_or_else(|| anyhow!("usage: mr get <key>"))?;
        let value = if key == "version" {
            self.config.pointer("/codebases/version").unwrap_or(&Value::Null)
        } else {
            self.config.get(key).unwrap_or(&Value::Null)
        };
        println!("config {key} , value: {value}");
        Ok(())
    }

    fn handle_merge(&self, args: &[String]) -> Result<()> {
        if let Some(web_url) = args.first() {
            return self.merge_gitlab_mr(web_url);
        }
        let project_name = current_project_name()?;
        println!("Current project_name: {project_name}\n");
        let current_branch = current_branch()?;
        if is_conflict_branch(&current_branch) {
            println!("Conflict resolving branch detected.\nMerge request will be created and merged automatically\n");
            let web_url = self
                .create_merge_request_from_conflict_branch(&current_branch, &project_name)?
                .ok_or_else(|| anyhow!("merge request was not created"))?;
            self.merge_gitlab_mr(&web_url)?;
            confirm_yes_or_exit(&format!("Do you want to remove local branch: {current_branch}? (Y/N)\n"))?;
            run_git(&["checkout", &get_source_branch_from_conflict_branch(&current_branch)?])?;
            run_git(&["branch", "-D", &current_branch])?;
        }
        Ok(())
    }

    fn handle_create(&self, args: &[String]) -> Result<()> {
        let project_name = current_project_name()?;
        println!("Current project_name: {project_name}\n");
        let current_branch = current_branch()?;

        if args.first().is_some_and(|v| v == "branch") {
            let branch = args.get(1).ok_or_else(|| anyhow!("missing branchName"))?;
            let reference = args.get(2).ok_or_else(|| anyhow!("missing ref"))?;
            println!("create {project_name} branch {branch} from {reference}\n");
            return self.create_branch_request(&project_name, branch, reference);
        }
        if args.first().is_some_and(|v| v == "tag") {
            let tag = args.get(1).ok_or_else(|| anyhow!("missing tagName"))?;
            let reference = args.get(2).ok_or_else(|| anyhow!("missing ref"))?;
            let message = args.get(3).map(String::as_str).unwrap_or("");
            println!("create {project_name} tag {tag} from {reference}\n");
            return self.create_tag_request(&project_name, tag, reference, message);
        }
        if is_conflict_branch(&current_branch) {
            println!("Conflict resolving branch detected.\nMerge request will be created automatically\n");
            self.create_merge_request_from_conflict_branch(&current_branch, &project_name)?;
            return Ok(());
        }

        let source_input = prompt_line(&format!(
            "Input source branch name, type `Enter` to use current branch '{current_branch}':\n"
        ))?;
        let (source_branch, need_push) = if source_input.trim().is_empty() {
            (current_branch.clone(), true)
        } else {
            (source_input.trim().to_string(), false)
        };

        let codebases = self
            .config
            .get("codebases")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("codebases config is missing"))?;
        if let Some(version) = codebases.get("version").and_then(Value::as_str) {
            println!("current version == {version}");
        }
        let keys = codebases.keys().cloned().collect::<Vec<_>>();
        let selected = choose("Choose codebase that you want to merge into", &keys)?;
        let codebase = &keys[selected];
        println!(
            "you choose is {} -- {}",
            codebase,
            codebases.get("version").unwrap_or(&Value::Null)
        );
        let target_branches = if codebase == "version" {
            vec![codebases
                .get("version")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("codebases.version is missing"))?
                .to_string()]
        } else {
            let choices = codebases
                .get(codebase)
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("codebases.{codebase} must be array"))?
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            choose_many("Choose target branch", &choices)?
                .into_iter()
                .map(|i| choices[i].clone())
                .collect()
        };
        if target_branches.is_empty() {
            return Ok(());
        }
        let project_id = self.get_project_id(&project_name)?;
        if need_push {
            run_git(&["push", "origin", &current_branch])?;
        }
        println!("Creating merge requests...\n");
        for target in target_branches {
            self.create_merge_request(project_id, &source_branch, &target, false, true)?;
        }
        Ok(())
    }

    fn handle_build(&self, args: &[String]) -> Result<()> {
        match args {
            [cmd, rest @ ..] if cmd == "log" => {
                let (server_name, url) = if rest.len() >= 2 {
                    (Some(rest[0].as_str()), rest[1].as_str())
                } else {
                    (None, rest.first().ok_or_else(|| anyhow!("missing build_url"))?.as_str())
                };
                self.follow_jenkins_log_by_url(url, server_name)
            }
            [cmd, env] if cmd == "update" => self.jenkins_build_update(env),
            [cmd, env, key, value] if cmd == "set" => self.jenkins_build_update_config(env, key, value),
            [cmd, env, key] if cmd == "get" => self.jenkins_build_get_config(env, key),
            [cmd, env, project @ ..] if cmd == "status" => {
                let project = if let Some(project) = project.first() {
                    project.clone()
                } else {
                    current_project_name()?
                };
                self.get_project_last_build(env, &project, None)
            }
            [cmd, env, project @ ..] if cmd == "params" => {
                let project = if let Some(project) = project.first() {
                    project.clone()
                } else {
                    current_project_name()?
                };
                self.print_job_build_parameters(env, &project)
            }
            [env] => self.jenkins_build(env, &current_project_name()?, None, None),
            [env, project] => self.jenkins_build(env, project, None, None),
            [env, project, version, image_version, ..] => {
                self.jenkins_build(env, project, Some(version.as_str()), Some(image_version.as_str()))
            }
            _ => Ok(()),
        }
    }

    fn get_jenkins_servers(&self) -> Result<BTreeMap<String, Value>> {
        let jenkins = self.config.get("jenkins").ok_or_else(|| anyhow!("jenkins config is missing"))?;
        if let Some(servers) = jenkins.get("servers").and_then(Value::as_object) {
            return Ok(servers.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        }
        Ok(BTreeMap::from([("default".to_string(), jenkins.clone())]))
    }

    fn get_jenkins_by_env(&self, env: &str) -> Result<JenkinsSelection> {
        let jenkins = self.config.get("jenkins").ok_or_else(|| anyhow!("jenkins config is missing"))?;
        if let Some((server_name, env_name)) = env.split_once('/') {
            if let Some(server) = self.get_jenkins_servers()?.get(server_name) {
                if let Some(envs) = server.pointer(&format!("/env/{env_name}")) {
                    return Ok(JenkinsSelection {
                        server_name: server_name.to_string(),
                        server: server.clone(),
                        env_name: env_name.to_string(),
                        envs: envs.clone(),
                    });
                }
            }
        }
        if let Some(envs) = jenkins.pointer(&format!("/env/{env}")) {
            return Ok(JenkinsSelection {
                server_name: "default".into(),
                server: jenkins.clone(),
                env_name: env.into(),
                envs: envs.clone(),
            });
        }
        let mut matches = Vec::new();
        for (name, server) in self.get_jenkins_servers()? {
            if let Some(envs) = server.pointer(&format!("/env/{env}")).cloned() {
                matches.push(JenkinsSelection {
                    server_name: name,
                    server,
                    env_name: env.into(),
                    envs,
                });
            }
        }
        if matches.is_empty() {
            bail!("{env} has no config, please edit ~/.mr-config.json file to set");
        }
        if matches.len() == 1 {
            return Ok(matches.remove(0));
        }
        let choices = matches.iter().map(|m| m.server_name.clone()).collect::<Vec<_>>();
        let selected = choose(&format!("Choose Jenkins server for env {env}"), &choices)?;
        Ok(matches.remove(selected))
    }

    fn get_default_jenkins(&self) -> Result<(String, Value)> {
        let jenkins = self.config.get("jenkins").ok_or_else(|| anyhow!("jenkins config is missing"))?;
        let servers = self.get_jenkins_servers()?;
        if let Some(default) = jenkins.get("default").and_then(Value::as_str) {
            if let Some(server) = servers.get(default) {
                return Ok((default.to_string(), server.clone()));
            }
        }
        servers.into_iter().next().ok_or_else(|| anyhow!("jenkins servers config is empty"))
    }

    fn get_jenkins_by_url(&self, url: &str, server_name: Option<&str>) -> Result<(String, Value)> {
        let servers = self.get_jenkins_servers()?;
        if let Some(name) = server_name {
            return servers
                .get(name)
                .cloned()
                .map(|server| (name.to_string(), server))
                .ok_or_else(|| anyhow!("Jenkins server {name} not found"));
        }
        let normalized_url = url.trim_end_matches('/');
        for (name, server) in servers {
            let host = normalize_jenkins_host(server.get("host").and_then(Value::as_str).unwrap_or(""));
            if !host.is_empty() && normalized_url.starts_with(&host) {
                return Ok((name, server));
            }
        }
        self.get_default_jenkins()
    }

    fn jenkins_auth(&self, jenkins: &Value, request: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        request.basic_auth(str_field(jenkins, "username"), Some(str_field(jenkins, "password")))
    }

    fn get_jenkins_crumb(&self, jenkins: &Value) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let host = normalize_jenkins_host(str_field(jenkins, "host"));
        let response = self
            .jenkins_auth(jenkins, self.client.get(format!("{host}/crumbIssuer/api/json")))
            .send()?;
        if response.status().is_success() {
            let crumb = response.json::<Value>()?;
            if let (Some(field), Some(value)) = (
                crumb.get("crumbRequestField").and_then(Value::as_str),
                crumb.get("crumb").and_then(Value::as_str),
            ) {
                headers.insert(
                    reqwest::header::HeaderName::from_bytes(field.as_bytes())?,
                    HeaderValue::from_str(value)?,
                );
            }
        }
        Ok(headers)
    }

    fn get_job_parameter_definitions(&self, job_url: &str, jenkins: &Value) -> Result<Vec<Value>> {
        let tree = "property[parameterDefinitions[name,type,description,defaultParameterValue[value],choices]]";
        let response = self
            .jenkins_auth(
                jenkins,
                self.client
                    .get(format!("{}/api/json", job_url.trim_end_matches('/')))
                    .query(&[("tree", tree)]),
            )
            .send()?;
        if !response.status().is_success() {
            println!("get job parameters error {}: {job_url}", response.status());
            return Ok(Vec::new());
        }
        let body = response.json::<Value>()?;
        let mut defs = Vec::new();
        for prop in body.get("property").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            defs.extend(prop.get("parameterDefinitions").and_then(Value::as_array).cloned().unwrap_or_default());
        }
        Ok(defs)
    }

    fn choose_jenkins_job(&self, envs: &Value, project: &str) -> Result<Value> {
        let jobs = envs
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("env has no job, please run mr build update env to add job"))?;
        let mut matches = jobs
            .iter()
            .filter(|job| str_field(job, "name").contains(project))
            .cloned()
            .collect::<Vec<_>>();
        if matches.is_empty() {
            bail!("project {project} has no matched Jenkins job");
        }
        if matches.len() == 1 {
            return Ok(matches.remove(0));
        }
        let choices = matches.iter().map(|j| str_field(j, "url").to_string()).collect::<Vec<_>>();
        let selected = choose("Choose job that you want to build", &choices)?;
        println!("you choose is {}", choices[selected]);
        Ok(matches.remove(selected))
    }

    fn get_build_parameter_payload(
        &self,
        job_url: &str,
        envs: &Value,
        version: &str,
        image_version: Option<&str>,
        jenkins: &Value,
    ) -> Result<(BTreeMap<String, String>, Vec<Value>, String, String)> {
        let defs = self.get_job_parameter_definitions(job_url, jenkins)?;
        let branch_definition = envs
            .get("branch_param")
            .and_then(Value::as_str)
            .map(|name| json!({ "name": name }))
            .or_else(|| find_parameter_definition(&defs, BRANCH_PARAM_ALIASES));
        let image_definition = envs
            .get("image_param")
            .and_then(Value::as_str)
            .map(|name| json!({ "name": name }))
            .or_else(|| find_parameter_definition(&defs, IMAGE_PARAM_ALIASES));

        let branch_param = branch_definition
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("codeBranch")
            .to_string();
        let image_param = image_definition
            .as_ref()
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("imageVersion")
            .to_string();
        let image_value = image_version
            .map(ToString::to_string)
            .or_else(|| image_definition.as_ref().and_then(get_parameter_default).map(ToString::to_string));
        let mut payload = BTreeMap::new();
        payload.insert(branch_param.clone(), format_branch_value(version, envs));
        if let Some(image_value) = image_value {
            payload.insert(image_param.clone(), image_value);
        }
        Ok((payload, defs, branch_param, image_param))
    }

    fn print_job_build_parameters(&self, env: &str, project: &str) -> Result<()> {
        let selected = self.get_jenkins_by_env(env)?;
        let job = self.choose_jenkins_job(&selected.envs, project)?;
        let defs = self.get_job_parameter_definitions(str_field(&job, "url"), &selected.server)?;
        let branch_aliases = selected
            .envs
            .get("branch_param")
            .and_then(Value::as_str)
            .map(|v| vec![v])
            .unwrap_or_else(|| BRANCH_PARAM_ALIASES.to_vec());
        let image_aliases = selected
            .envs
            .get("image_param")
            .and_then(Value::as_str)
            .map(|v| vec![v])
            .unwrap_or_else(|| IMAGE_PARAM_ALIASES.to_vec());
        let branch_def = find_parameter_definition(&defs, &branch_aliases);
        let image_def = find_parameter_definition(&defs, &image_aliases);
        println!(
            "jenkins: {}, env: {}, job: {}",
            selected.server_name,
            selected.env_name,
            str_field(&job, "name")
        );
        println!(
            "branch param: {} default: {}",
            branch_def.as_ref().map(|v| str_field(v, "name")).unwrap_or("codeBranch"),
            branch_def.as_ref().and_then(get_parameter_default).unwrap_or("")
        );
        println!(
            "image param: {} default: {}",
            image_def.as_ref().map(|v| str_field(v, "name")).unwrap_or("imageVersion"),
            image_def.as_ref().and_then(get_parameter_default).unwrap_or("")
        );
        for def in defs {
            println!(
                "param name: {} type: {} default: {}",
                str_field(&def, "name"),
                str_field(&def, "type"),
                get_parameter_default(&def).unwrap_or("")
            );
        }
        Ok(())
    }

    fn get_queue_build_url(&self, queue_url: &str, jenkins: &Value, timeout_secs: u64) -> Result<Option<String>> {
        if queue_url.is_empty() {
            return Ok(None);
        }
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        println!("waiting Jenkins queue: {queue_url}");
        while Instant::now() < deadline {
            let response = self
                .jenkins_auth(jenkins, self.client.get(format!("{}/api/json", queue_url.trim_end_matches('/'))))
                .send()?;
            if !response.status().is_success() {
                println!("get queue item error {}: {}", response.status(), response.text()?);
                return Ok(None);
            }
            let queue = response.json::<Value>()?;
            if let Some(url) = queue.pointer("/executable/url").and_then(Value::as_str) {
                return Ok(Some(url.to_string()));
            }
            if let Some(why) = queue.get("why").and_then(Value::as_str) {
                println!("queue waiting: {why}");
            }
            thread::sleep(Duration::from_secs(2));
        }
        println!("wait Jenkins queue timeout: {queue_url}");
        Ok(None)
    }

    fn get_build_result(&self, build_url: &str, jenkins: &Value) -> Result<Option<String>> {
        let response = self
            .jenkins_auth(jenkins, self.client.get(format!("{}/api/json", build_url.trim_end_matches('/'))))
            .send()?;
        if !response.status().is_success() {
            return Ok(None);
        }
        Ok(response.json::<Value>()?.get("result").and_then(Value::as_str).map(ToString::to_string))
    }

    fn follow_jenkins_build_log(&self, build_url: Option<String>, jenkins: &Value) -> Result<()> {
        let Some(build_url) = build_url else {
            return Ok(());
        };
        println!("build url: {build_url}");
        println!("console log:");
        let mut start = 0usize;
        loop {
            let response = self
                .jenkins_auth(
                    jenkins,
                    self.client
                        .get(format!("{}/logText/progressiveText", build_url.trim_end_matches('/')))
                        .query(&[("start", start)]),
                )
                .send()?;
            if !response.status().is_success() {
                println!("get console log error {}: {}", response.status(), response.text()?);
                return Ok(());
            }
            let headers = response.headers().clone();
            let text = response.text()?;
            if !text.is_empty() {
                print!("{text}");
                io::stdout().flush()?;
            }
            if let Some(next) = headers.get("X-Text-Size").and_then(|v| v.to_str().ok()) {
                start = next.parse().unwrap_or(start);
            }
            let more = headers
                .get("X-More-Data")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == "true");
            if !more {
                break;
            }
            thread::sleep(Duration::from_secs(2));
        }
        println!("\nconsole log finished, build result: {:?}", self.get_build_result(&build_url, jenkins)?);
        Ok(())
    }

    fn follow_jenkins_log_by_url(&self, url: &str, server_name: Option<&str>) -> Result<()> {
        let (name, jenkins) = self.get_jenkins_by_url(url, server_name)?;
        let mut normalized = normalize_jenkins_build_url(url)?;
        println!("jenkins: {name}");
        if normalized.contains("/queue/item/") {
            normalized = self
                .get_queue_build_url(&normalized, &jenkins, 120)?
                .unwrap_or_default();
        }
        self.follow_jenkins_build_log(Some(normalized), &jenkins)
    }

    fn jenkins_build_update(&self, env: &str) -> Result<()> {
        let selected = self.get_jenkins_by_env(env)?;
        let view_name = str_field(&selected.envs, "viewName");
        let host = normalize_jenkins_host(str_field(&selected.server, "host"));
        let response = self
            .jenkins_auth(
                &selected.server,
                self.client
                    .get(format!("{host}/view/{}/api/json", urlencoding::encode(view_name))),
            )
            .send()?;
        if !response.status().is_success() {
            println!("update Jenkins jobs error {}: {}", response.status(), response.text()?);
            return Ok(());
        }
        let result = response.json::<Value>()?;
        let mut jobs = Vec::new();
        for job in result.get("jobs").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            println!("{}  {}", str_field(job, "name"), str_field(job, "url"));
            jobs.push(json!({
                "name": str_field(job, "name"),
                "url": str_field(job, "url"),
                "jenkins": selected.server_name,
            }));
        }
        let mut config = self.config.clone();
        if selected.server_name == "default" && config.pointer_mut(&format!("/jenkins/env/{}", selected.env_name)).is_some() {
            config["jenkins"]["env"][&selected.env_name]["jobs"] = Value::Array(jobs);
        } else {
            config["jenkins"]["servers"][&selected.server_name]["env"][&selected.env_name]["jobs"] = Value::Array(jobs);
        }
        save_config(&config)
    }

    fn jenkins_build(&self, env: &str, project: &str, version: Option<&str>, image_version: Option<&str>) -> Result<()> {
        let selected = self.get_jenkins_by_env(env)?;
        let version = version
            .map(ToString::to_string)
            .or_else(|| selected.envs.get("version").and_then(Value::as_str).map(ToString::to_string))
            .ok_or_else(|| anyhow!("version is null"))?;
        let image_version = image_version.or_else(|| selected.envs.get("image_version").and_then(Value::as_str));
        let job = self.choose_jenkins_job(&selected.envs, project)?;
        let job_url = str_field(&job, "url");
        let (payload, _, branch_param, image_param) =
            self.get_build_parameter_payload(job_url, &selected.envs, &version, image_version, &selected.server)?;
        let headers = self.get_jenkins_crumb(&selected.server)?;
        let response = self
            .jenkins_auth(
                &selected.server,
                self.client
                    .post(format!("{}/buildWithParameters", job_url.trim_end_matches('/')))
                    .headers(headers)
                    .query(&payload),
            )
            .send()?;
        if response.status().as_u16() == 201 {
            println!(
                "building job {job_url}, jenkins: {}, version: {}, image: {}",
                selected.server_name,
                payload.get(&branch_param).map(String::as_str).unwrap_or(""),
                payload.get(&image_param).map(String::as_str).unwrap_or("")
            );
            let queue_url = response
                .headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let timeout = selected.envs.get("queue_timeout").and_then(Value::as_u64).unwrap_or(120);
            let build_url = self.get_queue_build_url(&queue_url, &selected.server, timeout)?;
            self.follow_jenkins_build_log(build_url, &selected.server)?;
        } else {
            println!("build error return code {}: {}", response.status(), response.text()?);
            return Ok(());
        }
        self.get_project_last_build(env, project, Some(job_url))
    }

    fn get_project_last_build(&self, env: &str, project: &str, job_url: Option<&str>) -> Result<()> {
        let selected = self.get_jenkins_by_env(env)?;
        let job_url = if let Some(job_url) = job_url {
            job_url.to_string()
        } else {
            str_field(&self.choose_jenkins_job(&selected.envs, project)?, "url").to_string()
        };
        let response = self
            .jenkins_auth(
                &selected.server,
                self.client.get(format!("{}/lastBuild/api/json?pretty=true", job_url.trim_end_matches('/'))),
            )
            .send()?;
        if response.status().is_success() {
            let last = response.json::<Value>()?;
            let mut param_map = BTreeMap::new();
            for action in last.get("actions").and_then(Value::as_array).unwrap_or(&Vec::new()) {
                for p in action.get("parameters").and_then(Value::as_array).unwrap_or(&Vec::new()) {
                    param_map.insert(str_field(p, "name").to_string(), p.get("value").cloned().unwrap_or(Value::Null));
                }
            }
            let ts = last.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
            let time = Local.timestamp_millis_opt(ts).single().map(|t| t.to_string()).unwrap_or_default();
            println!(
                "last build status: {} jenkins: {} build time: {} params: {:?} url: {}",
                last.get("result").unwrap_or(&Value::Null),
                selected.server_name,
                time,
                param_map,
                str_field(&last, "url")
            );
        }
        Ok(())
    }

    fn jenkins_build_update_config(&self, env: &str, key: &str, value: &str) -> Result<()> {
        let selected = self.get_jenkins_by_env(env)?;
        let mut config = self.config.clone();
        if selected.server_name == "default" && config.pointer_mut(&format!("/jenkins/env/{}", selected.env_name)).is_some() {
            config["jenkins"]["env"][&selected.env_name][key] = Value::String(value.to_string());
        } else {
            config["jenkins"]["servers"][&selected.server_name]["env"][&selected.env_name][key] =
                Value::String(value.to_string());
        }
        save_config(&config)
    }

    fn jenkins_build_get_config(&self, env: &str, key: &str) -> Result<()> {
        let selected = self.get_jenkins_by_env(env)?;
        println!(
            "build config key:{key}, value: {}",
            selected.envs.get(key).unwrap_or(&Value::Null)
        );
        Ok(())
    }
}

fn print_help() {
    println!("create       default create a merge\n             or create branch branchName ref\n             or create tag tagName ref message");
    println!("list         show all merge request");
    println!("merge        merge url, if not arg default merge current conflict branch");
    println!("projects     show all projects");
    println!("add          add current project to mr manage");
    println!("remove       remove current project out mr manage");
    println!("set          set current version, ex. set version version/v5.1.0, or set release uat ");
    println!("get          get current version, ex. set version");
    println!("change       show all have current version change, ex. set version version/v5.1.0 ");
    println!("release      create all have version branch to release version");
    println!("build        jenkins build, build update/status/params/log/set/get");
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(PathBuf::from(home).join(".mr-config.json"))
}

fn load_config() -> Result<Value> {
    let path = config_path()?;
    let raw = fs::read_to_string(&path).with_context(|| format!("config file was not found: {}", path.display()))?;
    let config: Value = serde_json::from_str(&raw)?;
    check_config(&config)?;
    Ok(config)
}

fn save_config(config: &Value) -> Result<()> {
    let path = config_path()?;
    fs::write(&path, serde_json::to_string_pretty(config)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn check_config(config: &Value) -> Result<()> {
    if config.is_null() {
        bail!("the config file is empty");
    }
    if config.get("gitlab_url").and_then(Value::as_str).unwrap_or("").is_empty() {
        bail!("the config is missing: gitlab_url ");
    }
    if config.get("gitlab_token").and_then(Value::as_str).unwrap_or("").is_empty() {
        bail!(
            "the config is missing: gitlab_token\nyou can get your token at this page: {}/-/profile/personal_access_tokens",
            config.get("gitlab_url").and_then(Value::as_str).unwrap_or("")
        );
    }
    if config.get("codebases").is_none() {
        bail!("the config is missing: codebases");
    }
    Ok(())
}

fn current_project_name() -> Result<String> {
    let url = project_repo_url()?;
    let name = url
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        bail!("It is not in a gitlab project directory");
    }
    Ok(name)
}

fn project_repo_url() -> Result<String> {
    let output = Command::new("git").args(["remote", "get-url", "origin"]).output()?;
    if !output.status.success() {
        bail!("It is not in a gitlab project directory");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn current_branch() -> Result<String> {
    let output = Command::new("git").args(["branch", "--show-current"]).output()?;
    if !output.status.success() {
        bail!("failed to get current git branch");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_git(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("git {:?} failed with {status}", args);
    }
    Ok(())
}

fn is_conflict_branch(branch: &str) -> bool {
    branch.starts_with("conflict/") && branch.contains("--conflict-to--") && get_target_branch(branch).is_ok()
}

fn get_target_branch(branch: &str) -> Result<String> {
    branch
        .rsplit("--conflict-to--")
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("invalid conflict branch"))
}

fn get_source_branch_from_conflict_branch(branch: &str) -> Result<String> {
    branch
        .strip_prefix("conflict/")
        .and_then(|s| s.split("--conflict-to--").next())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("Invalid input string"))
}

fn parse_mr_url(web_url: &str) -> Result<(String, String, String)> {
    let (base, iid) = web_url
        .trim()
        .split_once("/-/merge_requests/")
        .ok_or_else(|| anyhow!("Invalid merge request URL"))?;
    let project = base.rsplit('/').next().unwrap_or("").to_string();
    Ok((base.to_string(), iid.to_string(), project))
}

fn normalize_jenkins_host(host: &str) -> String {
    host.trim_end_matches('/').to_string()
}

fn normalize_jenkins_build_url(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url.trim())?;
    let mut normalized = format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or(""),
        parsed.path()
    )
    .trim_end_matches('/')
    .to_string();
    for suffix in ["/consoleFull", "/console", "/consoleText", "/logText/progressiveText"] {
        if normalized.ends_with(suffix) {
            normalized.truncate(normalized.len() - suffix.len());
            break;
        }
    }
    Ok(normalized)
}

fn find_parameter_definition(defs: &[Value], aliases: &[&str]) -> Option<Value> {
    for alias in aliases {
        if let Some(found) = defs.iter().find(|d| d.get("name").and_then(Value::as_str) == Some(*alias)) {
            return Some(found.clone());
        }
    }
    let lowered = aliases.iter().map(|v| v.to_ascii_lowercase()).collect::<Vec<_>>();
    defs.iter()
        .find(|d| {
            let name = str_field(d, "name").to_ascii_lowercase();
            lowered.iter().any(|alias| name.contains(alias))
        })
        .cloned()
}

fn get_parameter_default(def: &Value) -> Option<&str> {
    def.pointer("/defaultParameterValue/value")
        .and_then(Value::as_str)
}

fn format_branch_value(version: &str, envs: &Value) -> String {
    let prefix = envs.get("branch_prefix").and_then(Value::as_str).unwrap_or("origin/");
    if !prefix.is_empty() && !version.starts_with(prefix) {
        format!("{prefix}{version}")
    } else {
        version.to_string()
    }
}

fn choose(message: &str, choices: &[String]) -> Result<usize> {
    if choices.is_empty() {
        bail!("no choices available");
    }
    println!("{message}");
    for (i, choice) in choices.iter().enumerate() {
        println!("{}. {}", i + 1, choice);
    }
    loop {
        let input = prompt_line("> ")?;
        if let Ok(n) = input.trim().parse::<usize>() {
            if (1..=choices.len()).contains(&n) {
                return Ok(n - 1);
            }
        }
        println!("Please input 1-{}", choices.len());
    }
}

fn choose_many(message: &str, choices: &[String]) -> Result<Vec<usize>> {
    println!("{message}");
    for (i, choice) in choices.iter().enumerate() {
        println!("{}. {}", i + 1, choice);
    }
    println!("Input numbers separated by comma, or Enter to select all checked defaults:");
    let input = prompt_line("> ")?;
    if input.trim().is_empty() {
        return Ok((0..choices.len()).collect());
    }
    let mut selected = Vec::new();
    for part in input.split(',') {
        let n = part.trim().parse::<usize>()?;
        if (1..=choices.len()).contains(&n) {
            selected.push(n - 1);
        }
    }
    Ok(selected)
}

fn prompt_line(message: &str) -> Result<String> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim_end_matches(['\r', '\n']).to_string())
}

fn confirm_yes_or_exit(message: &str) -> Result<()> {
    let input = prompt_line(message)?;
    if matches!(input.to_ascii_uppercase().as_str(), "N" | "NO") {
        println!("You choose no, bye");
        std::process::exit(0);
    }
    Ok(())
}

fn red(text: &str) -> String {
    format!("\x1b[91m{text}\x1b[0m")
}

fn green(text: &str) -> String {
    format!("\x1b[32m{text}\x1b[0m")
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("{key} is missing"))
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number_field(value: &Value, key: &str) -> Result<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("{key} is missing or not a number"))
}

fn config_array<'a>(config: &'a Value, key: &str) -> &'a [Value] {
    config.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

fn ensure_array_mut<'a>(config: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>> {
    if config.get(key).is_none() {
        config[key] = Value::Array(Vec::new());
    }
    config
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("{key} must be array"))
}
