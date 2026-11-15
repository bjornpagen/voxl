fn main() -> std::process::ExitCode {
    match voxl::run(std::env::args().skip(1)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("voxl: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
