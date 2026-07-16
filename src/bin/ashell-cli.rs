fn main() {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "client") {
        args.remove(0);
    }
    std::process::exit(ashell::client::run_blocking_from(args));
}
