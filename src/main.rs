fn main() {
    if let Err(e) = gh_dags::cli::run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
