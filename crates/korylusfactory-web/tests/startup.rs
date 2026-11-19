use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_korylusfactory-web"))
}

fn run(args: &[&str]) -> std::process::Output {
    command()
        .args(args)
        .output()
        .expect("ウェブアプリを実行できなかった")
}

/// 空いているポートで待ち受け、`database` のDBを使う設定ファイルを `dir` に書き、そのパスを返す。
fn write_config(dir: &Path, database: &Path) -> PathBuf {
    let config = dir.join("config.json");
    std::fs::write(
        &config,
        format!(
            r#"{{"listen": "127.0.0.1:0", "database": {}, "owners": ["owner@example.com"]}}"#,
            serde_json::to_string(database).expect("パスをJSONにできなかった")
        ),
    )
    .expect("設定ファイルを書けなかった");
    config
}

/// 起動したウェブアプリ。テストが途中で失敗しても残さないよう、手放すときに止める。
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        // すでに終わっていると失敗するが、止める相手がいないだけなので結果を見ない。
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn 設定ファイルが無ければエラーを出して失敗する() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
    let path = dir.path().join("missing.json");

    let output = run(&["--config", path.to_str().expect("パスが文字列ではない")]);

    assert!(!output.status.success(), "設定ファイルが無いのに成功した");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("エラー: 設定ファイル") && stderr.contains("を読めません"),
        "設定ファイルを読めないことを伝えていない: {stderr}"
    );
}

#[test]
fn dbを開けなければエラーを出して失敗する() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
    let config = write_config(
        dir.path(),
        &dir.path().join("missing").join("factory.sqlite3"),
    );

    let output = run(&["--config", config.to_str().expect("パスが文字列ではない")]);

    assert!(!output.status.success(), "DBを開けないのに成功した");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("エラー: DB"),
        "DBを開けないことを伝えていない: {stderr}"
    );
}

#[test]
fn 待ち受けを始めたあとにsigtermを受け取ると開いているsseを閉じて止め_成功で終わる() {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作れなかった");
    let config = write_config(dir.path(), &dir.path().join("factory.sqlite3"));
    let mut running = Running(
        command()
            .args(["--config", config.to_str().expect("パスが文字列ではない")])
            .stderr(Stdio::piped())
            .spawn()
            .expect("ウェブアプリを起動できなかった"),
    );
    // 起動と停止の両方で、子プロセスが応答しない場合にテストを打ち切る。
    let pid = rustix::process::Pid::from_child(&running.0);
    let (finished, watch) = mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if watch.recv_timeout(Duration::from_secs(30)) == Err(mpsc::RecvTimeoutError::Timeout) {
            // すでに終わっているときは止める必要が無い。
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    });
    let stderr = running
        .0
        .stderr
        .take()
        .expect("標準エラーを受け取れなかった");
    let mut lines = BufReader::new(stderr)
        .lines()
        .map(|line| line.expect("標準エラーを読めなかった"));

    // 行を読み終わるまで待つため、待ち受けを始めるまでの時間を決めずに済む。
    let address = lines
        .by_ref()
        .find_map(|line| {
            line.strip_prefix("korylusfactory-web: ")?
                .strip_suffix(" で待ち受けています")
                .map(str::to_owned)
        })
        .expect("待ち受けを始める前に終わった");
    let address = address.parse().expect("待ち受けのアドレスを読めなかった");
    let mut events = open_events(&address);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
        .expect("SIGTERMを送れなかった");
    let rest: Vec<String> = lines.collect();
    let status = running.0.wait().expect("終わるのを待てなかった");
    // 見張りが先に終わっていると送れないが、そのときは強制終了したあとなので結果を見ない。
    let _ = finished.send(());
    watchdog.join().expect("見張りのスレッドが止まった");

    assert!(
        status.success(),
        "SIGTERMを受け取って成功で終わらなかった: {status}"
    );
    assert!(
        rest.iter().any(|line| line.contains("止めます")),
        "止めることを伝えていない: {rest:?}"
    );
    let mut remaining = Vec::new();
    events
        .read_to_end(&mut remaining)
        .expect("SSEの残りを読めなかった");
}

/// addressのウェブアプリでSSEを開き、応答のヘッダーを読み終えた接続を返す。
fn open_events(address: &SocketAddr) -> BufReader<TcpStream> {
    let timeout = Duration::from_secs(5);
    let mut stream =
        TcpStream::connect_timeout(address, timeout).expect("ウェブアプリにつなげなかった");
    stream
        .set_read_timeout(Some(timeout))
        .expect("SSEの読み取りの上限を設定できなかった");
    stream
        .set_write_timeout(Some(timeout))
        .expect("SSEの書き込みの上限を設定できなかった");
    write!(
        stream,
        "GET /events HTTP/1.1\r\nHost: {address}\r\nX-ExeDev-Email: owner@example.com\r\n\r\n"
    )
    .expect("要求を送れなかった");
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).expect("応答を読めなかった");
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "SSEを開けなかった: {status}"
    );
    let mut sse = false;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .expect("応答のヘッダーを読めなかった");
        assert!(read > 0, "応答のヘッダーが終わる前に切断された");
        if line == "\r\n" {
            break;
        }
        sse |= line
            .trim_end()
            .eq_ignore_ascii_case("content-type: text/event-stream");
    }
    assert!(sse, "SSEのContent-Typeが返らなかった");
    reader
}
