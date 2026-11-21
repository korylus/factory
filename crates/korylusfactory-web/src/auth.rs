//! 要求の主 (人間・ディレクター) の見分けと、呼べないものの拒否。
//!
//! ウェブアプリはexe.devのプロキシの内側で動き、プロキシが付けるヘッダーで要求の主を見分ける。
//! プロキシは、利用者が自分で付けた `X-ExeDev-Email` を正しい値に置き換え、`X-Exedev-Source-Vm` をpeer連携の経路でだけ付ける。
//!
//! - 人間: `X-Exedev-Source-Vm` が無く、`X-ExeDev-Email` が設定の `owners` のどれか
//! - ディレクター: `X-Exedev-Source-Vm` が設定の `directors` にあるVM名
//!
//! peer連携の要求にも持ち主の `X-ExeDev-Email` が付くため、`X-Exedev-Source-Vm` がある要求は人間として扱わない。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::api::{self, Problem};
use crate::config::{Config, DevIdentity};
use crate::page;

/// exe.devが、ログインしている人のメールアドレスを付けるヘッダー。
pub const EMAIL_HEADER: HeaderName = HeaderName::from_static("x-exedev-email");

/// exe.devが、peer連携で呼び出し元のVM名を付けるヘッダー。
pub const SOURCE_VM_HEADER: HeaderName = HeaderName::from_static("x-exedev-source-vm");

/// ブラウザが、要求を送らせたページのオリジンと送り先との関係を付けるヘッダー。
pub const SEC_FETCH_SITE_HEADER: HeaderName = HeaderName::from_static("sec-fetch-site");

/// 401の応答に付ける認証の要求。exe.devのプロキシが返すものと同じにする。
const CHALLENGE: &str =
    r#"Bearer realm="exe.dev", error_uri="https://exe.dev/docs/https-tokens-for-vms""#;

/// 見分けた要求の主。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// 画面を見る人間。
    Human {
        /// exe.devでログインしている人のメールアドレス。
        email: String,
    },
    /// 状態を送るディレクター。
    Director {
        /// ディレクターが動くVMの名前。
        vm: String,
        /// 設定でVM名に対応付けたプロジェクトの名前。要求の本文ではなく、こちらを使う。
        project: String,
    },
}

/// 要求を拒否した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// 主を示すヘッダーが無い。
    Unauthenticated,
    /// 主は分かったが、知らない人やVMか、その主が呼べないものを呼んだ。
    Forbidden,
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        match self {
            Self::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, CHALLENGE)],
                "ログインしてください。\n",
            )
                .into_response(),
            Self::Forbidden => {
                (StatusCode::FORBIDDEN, "この操作は許可されていません。\n").into_response()
            }
        }
    }
}

impl Rejection {
    /// 拒否の応答を返す。APIのパスへの要求にはProblem Detailsで返す。
    pub fn respond(self, path: &str) -> Response {
        if !api::is_api(path) {
            return self.into_response();
        }
        match self {
            Self::Unauthenticated => {
                let mut response = Problem::unauthenticated().into_response();
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static(CHALLENGE),
                );
                response
            }
            Self::Forbidden => Problem::forbidden().into_response(),
        }
    }
}

/// 要求のヘッダーから主を見分ける。
///
/// 設定に開発用の身元があるときは、ヘッダーを見ずにその身元を主として見分ける。
pub fn identify(config: &Config, headers: &HeaderMap) -> Result<Principal, Rejection> {
    let (email, vm) = match &config.dev_identity {
        Some(DevIdentity::Email(email)) => (Some(email.as_str()), None),
        Some(DevIdentity::Vm(vm)) => (None, Some(vm.as_str())),
        None => (
            single(headers, &EMAIL_HEADER)?,
            single(headers, &SOURCE_VM_HEADER)?,
        ),
    };
    if let Some(vm) = vm {
        let director = config.director(vm).ok_or(Rejection::Forbidden)?;
        return Ok(Principal::Director {
            vm: director.vm.clone(),
            project: director.project.clone(),
        });
    }
    match email {
        Some(email) if config.is_owner(email) => Ok(Principal::Human {
            email: email.to_owned(),
        }),
        Some(_) => Err(Rejection::Forbidden),
        None => Err(Rejection::Unauthenticated),
    }
}

/// ヘッダーの値を1つだけ取り出す。無ければ `None` を返す。
///
/// exe.devのプロキシは同じヘッダーを重ねて付けないため、2つ以上あるか文字列として読めない要求は、プロキシを通っていないものとして拒否する。
fn single<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a str>, Rejection> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Rejection::Forbidden);
    }
    value.to_str().map(Some).map_err(|_| Rejection::Forbidden)
}

/// すべての要求の主を見分け、見分けられない要求を拒否するミドルウェア。
///
/// 見分けた主は、要求の拡張に [`Principal`] として入れる。
pub async fn authenticate(
    State(config): State<Arc<Config>>,
    mut request: Request,
    next: Next,
) -> Response {
    match identify(&config, request.headers()) {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(rejection) => rejection.respond(request.uri().path()),
    }
}

/// 人間だけが呼べるようにするミドルウェア。[`authenticate`] の内側に置く。
pub async fn require_human(request: Request, next: Next) -> Response {
    match request.extensions().get::<Principal>() {
        Some(Principal::Human { .. }) => next.run(request).await,
        _ => Rejection::Forbidden.respond(request.uri().path()),
    }
}

/// 人間の状態を変える要求 (GET・HEAD以外) を、同じオリジンから送られたものだけに限るミドルウェア。
///
/// 人間の認証はexe.devのプロキシのCookieで行い、その属性はウェブアプリから決められない。
/// ほかのサイトに置かれたフォームから送らせた要求 (CSRF) を拒むため、ブラウザが付ける `Sec-Fetch-Site` が `same-origin` の要求だけを通し、それ以外は403の画面を返す。
/// [`require_human`] の内側に置く。
pub async fn require_same_origin(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if safe || single(request.headers(), &SEC_FETCH_SITE_HEADER) == Ok(Some("same-origin")) {
        return next.run(request).await;
    }
    page::forbidden()
}

/// ディレクターだけが呼べるようにするミドルウェア。[`authenticate`] の内側に置く。
pub async fn require_director(request: Request, next: Next) -> Response {
    match request.extensions().get::<Principal>() {
        Some(Principal::Director { .. }) => next.run(request).await,
        _ => Rejection::Forbidden.respond(request.uri().path()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::http::HeaderValue;

    use super::*;

    fn config(dev_identity: &str) -> Config {
        Config::parse(
            Path::new("config.json"),
            &format!(
                r#"{{
                    "database": "factory.sqlite3",
                    "owners": ["owner@example.com"],
                    "directors": [{{"vm": "annict-factory", "project": "Annict"}}]
                    {dev_identity}
                }}"#
            ),
        )
        .expect("設定を読み取れなかった")
    }

    fn headers(pairs: &[(&HeaderName, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                (*name).clone(),
                HeaderValue::from_str(value).expect("ヘッダーの値を作れなかった"),
            );
        }
        headers
    }

    #[test]
    fn 設定にあるメールアドレスの要求は人間として見分ける() {
        let got = identify(
            &config(""),
            &headers(&[(&EMAIL_HEADER, "owner@example.com")]),
        );

        assert_eq!(
            got,
            Ok(Principal::Human {
                email: "owner@example.com".to_owned()
            }),
            "人間として見分けなかった"
        );
    }

    #[test]
    fn 設定にあるvm名の要求はディレクターとして見分ける() {
        let got = identify(
            &config(""),
            &headers(&[
                (&SOURCE_VM_HEADER, "annict-factory"),
                (&EMAIL_HEADER, "owner@example.com"),
            ]),
        );

        assert_eq!(
            got,
            Ok(Principal::Director {
                vm: "annict-factory".to_owned(),
                project: "Annict".to_owned()
            }),
            "peer連携の持ち主のメールアドレスが付いていても、ディレクターとして見分けなかった"
        );
    }

    #[test]
    fn 主を示すヘッダーが無い要求は401で拒否する() {
        let got = identify(&config(""), &HeaderMap::new());

        assert_eq!(
            got,
            Err(Rejection::Unauthenticated),
            "未認証として拒否しなかった"
        );
    }

    #[test]
    fn 知らない人や知らないvmの要求は403で拒否する() {
        for pairs in [
            vec![(&EMAIL_HEADER, "other@example.com")],
            vec![(&SOURCE_VM_HEADER, "other-vm")],
            vec![
                (&SOURCE_VM_HEADER, "other-vm"),
                (&EMAIL_HEADER, "owner@example.com"),
            ],
        ] {
            let got = identify(&config(""), &headers(&pairs));

            assert_eq!(
                got,
                Err(Rejection::Forbidden),
                "知らない主を拒否しなかった: {pairs:?}"
            );
        }
    }

    #[test]
    fn 同じヘッダーが重なった要求は403で拒否する() {
        let got = identify(
            &config(""),
            &headers(&[
                (&EMAIL_HEADER, "owner@example.com"),
                (&EMAIL_HEADER, "owner@example.com"),
            ]),
        );

        assert_eq!(
            got,
            Err(Rejection::Forbidden),
            "重なったヘッダーを受け付けた"
        );
    }

    #[test]
    fn 文字列として読めないヘッダーは403で拒否する() {
        let config = config("");
        let unreadable = HeaderValue::from_bytes(&[0x80]).expect("ヘッダーの値を作れなかった");

        for name in [EMAIL_HEADER, SOURCE_VM_HEADER] {
            let mut headers = HeaderMap::new();
            headers.insert(&name, unreadable.clone());

            assert_eq!(
                identify(&config, &headers),
                Err(Rejection::Forbidden),
                "読めない {name} を403で拒否しなかった"
            );
        }
    }

    #[test]
    fn 開発用の身元があるときはヘッダーを見ずにその身元とみなす() {
        let human = config(r#", "dev_identity": {"email": "owner@example.com"}"#);
        let director = config(r#", "dev_identity": {"vm": "annict-factory"}"#);
        let stranger = config(r#", "dev_identity": {"email": "other@example.com"}"#);

        assert!(
            matches!(
                identify(&human, &HeaderMap::new()),
                Ok(Principal::Human { .. })
            ),
            "開発用の身元の人間とみなさなかった"
        );
        assert!(
            matches!(
                identify(&director, &headers(&[(&EMAIL_HEADER, "owner@example.com")])),
                Ok(Principal::Director { .. })
            ),
            "開発用の身元のディレクターとみなさなかった"
        );
        assert_eq!(
            identify(&stranger, &HeaderMap::new()),
            Err(Rejection::Forbidden),
            "設定に無い開発用の身元を受け付けた"
        );
    }
}
