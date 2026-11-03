// Telegram 개인 대화를 OFFICESTRA 직원에게 잇는 독립 실행 파일. Node scripts/office-telegram.mjs를 대신하며 4317과 앱을 재시작하지 않는다.

use officestra_backend::telegram_bridge::cli::{CliOptions, run};

fn main() {
    let code = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(run(CliOptions::from_environment())),
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    };
    std::process::exit(code);
}
