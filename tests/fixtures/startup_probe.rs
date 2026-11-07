fn main() {
    let path = std::env::var_os("ZERQON_TEST_PROBE_PID").expect("probe marker");
    std::fs::write(path, std::process::id().to_string()).expect("write probe marker");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
