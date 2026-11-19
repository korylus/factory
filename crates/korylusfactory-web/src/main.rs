use std::error::Error;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use korylusfactory_web::app::{self, AppState};
use korylusfactory_web::config::{Config, DevIdentity};
use korylusfactory_web::db::Database;
use korylusfactory_web::describe;
use korylusfactory_web::events::Events;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

/// Korylus Factoryの管制のウェブアプリ。
#[derive(Parser)]
#[command(name = "korylusfactory-web", version)]
struct Cli {
    /// 設定ファイル (JSON) のパス
    #[arg(long)]
    config: PathBuf,
}

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() == log::Level::Error
            && metadata.target().starts_with("korylusfactory_web::")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            // 標準エラーへ書けなくても、処理中のHTTP応答は返す。
            let _ = writeln!(
                std::io::stderr().lock(),
                "korylusfactory-web: エラー: {}",
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("korylusfactory-web: エラー: {}", describe(err.as_ref()));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), Box<dyn Error>> {
    log::set_logger(&StderrLogger)?;
    log::set_max_level(log::LevelFilter::Error);
    let config = Config::load(&cli.config)?;
    if let Some(identity) = &config.dev_identity {
        let identity = match identity {
            DevIdentity::Email(email) => format!("人間 {email}"),
            DevIdentity::Vm(vm) => format!("ディレクター {vm}"),
        };
        eprintln!(
            "korylusfactory-web: 警告: 開発用の身元が有効です。すべての要求を{identity}からのものとみなします。本番では「dev_identity」を書かないでください"
        );
    }
    let database = Database::open(&config.database)?;
    let listen = config.listen;
    let events = Events::new();
    let app = app::router(AppState {
        config: Arc::new(config),
        database,
        events: events.clone(),
    });
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        // 待ち受けの行を出した直後に届いた合図でも処理中の要求を終えて止まるよう、待ち受ける前にリスナーを登録する。
        let shutdown = shutdown();
        let listener = TcpListener::bind(listen).await?;
        // ポートに0を書いたときも、実際に待ち受けたポートを伝える。
        let address = listener.local_addr()?;
        eprintln!("korylusfactory-web: {address} で待ち受けています");
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown.await;
                // SSEは終わらない応答のため、閉じてから処理中の要求を待つ。
                events.close();
            })
            .await
    })?;
    Ok(())
}

/// 止める合図 (Ctrl+CかSIGTERM) のリスナーを登録し、どちらかを受け取るまで待つfutureを返す。
///
/// systemdはSIGTERMで止めるため、両方を受け取り、処理中の要求を終えてから止める。
/// 非同期のランタイムの中で呼ぶ。
fn shutdown() -> impl Future<Output = ()> {
    let interrupt = receive(SignalKind::interrupt(), "Ctrl+C");
    let terminate = receive(SignalKind::terminate(), "SIGTERM");
    async move {
        tokio::select! {
            () = interrupt => {}
            () = terminate => {}
        }
        eprintln!("korylusfactory-web: 止めます");
    }
}

/// 合図のリスナーをその場で登録し、合図を受け取るまで待つfutureを返す。
///
/// 登録できないときは警告を出し、終わらないfutureを返す。もう片方の合図で止められるようにするため。
fn receive(kind: SignalKind, name: &str) -> impl Future<Output = ()> + use<> {
    let listener = signal(kind)
        .inspect_err(|err| eprintln!("korylusfactory-web: 警告: {name}を受け取れません: {err}"))
        .ok();
    async move {
        match listener {
            Some(mut listener) => {
                listener.recv().await;
            }
            None => std::future::pending().await,
        }
    }
}
