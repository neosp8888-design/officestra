// 직원 응답을 음성으로 따라 읽는 독립 실행 파일. Node backend/src/voice/voice-follow.mjs를 대신하며, 백엔드가 '--follow 직원ID'로 띄우거나 사람이 --replay 등으로 직접 실행한다.

use officestra_backend::voice::follow::run;

fn main() {
    let arguments: Vec<String> = std::env::args_os().skip(1).map(|argument| argument.to_string_lossy().into_owned()).collect();
    let code = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime.block_on(run(arguments)),
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    };
    std::process::exit(code);
}
