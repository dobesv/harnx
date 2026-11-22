//! Print a key file for explicit operator-managed storage.

fn main() -> std::process::ExitCode {
    match harnx_credentials::Keyring::generate_key_file() {
        Ok(json) => {
            println!("{}", json.expose());
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
