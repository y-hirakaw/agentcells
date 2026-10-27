//! cellとして動くAIが、Webのページを読むためのコマンド（`agentcells cell fetch <URL>`）。
//!
//! Claude CodeのWebFetchはsandboxを通らず、読めた秘密を外へ送る道になるので使わせない。代わりに、
//! productの `[product.sandbox]` の `network` に書いたホストだけを、httpsのGETで開く。
//!
//! このコマンドもsandboxの中で動くので、ほかのホストへの通信はプロキシ（`strictAllowlist`）が止める。
//! ここでの照合は、断るときに足すとよい設定を案内するためと、二重の守りのため。
//! 照合はsandboxと同じ決まりにする: 大文字・小文字を問わず、`example.com` はそのホストだけ、
//! `*.example.com` はそのサブドメインだけ（`example.com` そのものは含まない）。
//!
//! 断ったホストは、sandboxが止めたときと同じ形（`deny network-outbound <ホスト>:443`）で出す。
//! CLIがclaudeの出力から拾って、手元（tickの出力）にだけ出す（[`crate::blocked`]）。公開のコメントには書かせない。

use std::io::Read;
use std::process::{Command, Stdio};

use crate::actions::Context;
use crate::error::{Error, Result};

/// 1回に出す量の上限（バイト）。大きいページでコンテキストがあふれないようにする。続きは `--from` で読む。
pub const LIMIT: usize = 30_000;

/// リダイレクトを追う回数の上限。
const MAX_REDIRECTS: usize = 5;

/// 接続と応答を待つ上限。
const CONNECT_TIMEOUT_SECS: u32 = 10;
const MAX_TIME_SECS: u32 = 60;

/// 受け取る量の上限（バイト）。出すのは [`LIMIT`] までだが、`--from` で続きを読めるように、ここまでは受け取る。
const MAX_FILESIZE: usize = 20_000_000;

/// curlの `--write-out` で、標準エラーの最後に書かせる目印。応答の状態とリダイレクトの先を読む。
const STATUS_MARK: &str = "agentcells-fetch-status:";

/// URLを確かめ、開いてよいホスト（小文字）を返す。httpsでないもの、ホストの形でないもの、
/// `network` にないホストは断る。
pub fn permitted(url: &str, network: &[String]) -> Result<String> {
    let host = host_of(url)?;
    if network.iter().any(|pattern| matches(&host, pattern)) {
        return Ok(host);
    }
    let listed = if network.is_empty() {
        "このproductには、開けるホストがありません".to_string()
    } else {
        format!("開けるのは {} です", network.join("、"))
    };
    Err(Error::new(format!(
        "{host} は、このproductの [product.sandbox] の network にないので、開きませんでした\ndeny network-outbound {host}:443"
    ))
    .with_hint(format!(
        "{listed}。このページが作業にどうしても要るなら、needs-human にして、config.toml の [product.sandbox] の network に足すよう人間に頼んでください。comment には、ホストの名前は書かずに、何のために読みたかったかを書いてください（コメントは公開されることがあるため。断ったホストは、CLIが手元に出します）。足すかどうかは人間が決めます"
    )))
}

/// httpsのURLから、ホスト（小文字）を取り出す。ユーザー情報（`user@`）、443以外のポート、
/// ホストとして正しくない形（IPアドレスの角かっこ、%で書いたものなど）は断る。
fn host_of(url: &str) -> Result<String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Error::new("URLに空白や制御文字が入っています"));
    }
    let rest = url
        .get(..8)
        .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
        .map(|_| &url[8..])
        .ok_or_else(|| {
            Error::new(format!("{url} は開けません"))
                .with_hint("https:// で始まるURLだけを開けます")
        })?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains(['@', '\\']) {
        return Err(Error::new(format!(
            "{url} は開けません（ホストの前にユーザー名などが入っています）"
        )));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|p| p != "443") {
        return Err(Error::new(format!("{url} は開けません"))
            .with_hint("ポートは443（書かないとき）だけです"));
    }
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let valid = host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if !valid {
        return Err(Error::new(format!(
            "{url} は開けません（ホストの名前として読めません）"
        )));
    }
    Ok(host)
}

/// ホストが、`network` の1つ（`example.com` か `*.example.com`）に当たるか。sandboxと同じ決まりにする。
fn matches(host: &str, pattern: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(base) => host
            .strip_suffix(base)
            .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
        None => host == pattern,
    }
}

/// 取ってきたもの。
#[derive(Debug)]
struct Page {
    status: u16,
    /// リダイレクトの先（curlが絶対のURLにしたもの）
    location: Option<String>,
    /// 出す部分（`from` から [`LIMIT`] まで）
    body: Vec<u8>,
    /// 受け取った全体の量（バイト）
    total: usize,
}

/// URLを開き、中身を標準出力に出す。リダイレクトは、先も `network` にあるときだけ追う。
/// 出すのは `from` バイト目から [`LIMIT`] までで、残りがあれば、続きの読み方を標準エラーに出す。
/// 2xxなら0、それ以外は1を返す。
pub fn fetch(context: &Context, url: &str, from: usize) -> Result<i32> {
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        permitted(&current, &context.network)?;
        let page = transfer(&current, "=https", from, LIMIT, &[])?;
        if (300..400).contains(&page.status)
            && let Some(next) = page.location
        {
            eprintln!("agentcells: {current} から {next} にリダイレクトします");
            current = next;
            continue;
        }
        {
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(&page.body)
                .and_then(|_| out.flush())
                .map_err(|e| Error::new(format!("中身を出せません: {e}")))?;
        }
        if let Some(note) = remaining(&current, from, page.body.len(), page.total) {
            eprintln!("\n{note}");
        }
        if !(200..300).contains(&page.status) {
            eprintln!("\nagentcells: 応答は {} でした", page.status);
            return Ok(1);
        }
        return Ok(0);
    }
    Err(Error::new(format!(
        "リダイレクトが{MAX_REDIRECTS}回を超えたので、やめました"
    )))
}

/// 出していない部分があるときの案内。
fn remaining(url: &str, from: usize, shown: usize, total: usize) -> Option<String> {
    let end = from + shown;
    if end >= total {
        return None;
    }
    Some(format!(
        "agentcells: 長いので、{from}〜{end}バイト目だけを出しました（全体で{total}バイト）。続きは `agentcells cell fetch --from {end} {url}` で読めます"
    ))
}

/// curlで1回だけ開く（リダイレクトは追わない）。`protocols` はテストで手元のhttpのサーバーに向けるために分けている。
fn transfer(
    url: &str,
    protocols: &str,
    from: usize,
    limit: usize,
    env: &[(&str, &str)],
) -> Result<Page> {
    let connect_timeout = CONNECT_TIMEOUT_SECS.to_string();
    let max_time = MAX_TIME_SECS.to_string();
    let max_filesize = MAX_FILESIZE.to_string();
    let write_out = format!("%{{stderr}}\n{STATUS_MARK} %{{http_code}} %{{redirect_url}}\n");
    let mut child = Command::new("curl")
        .args([
            // ~/.curlrc を読まない（AIも書けるため）
            "--disable",
            "--silent",
            "--show-error",
            "--globoff",
            "--compressed",
            "--proto",
            protocols,
            "--max-redirs",
            "0",
            "--connect-timeout",
            &connect_timeout,
            "--max-time",
            &max_time,
            "--max-filesize",
            &max_filesize,
            "--user-agent",
            concat!("agentcells/", env!("CARGO_PKG_VERSION")),
            "--write-out",
            &write_out,
            "--url",
            url,
        ])
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            Error::new(format!("curlを動かせません: {e}"))
                .with_hint("人間に回してください（needs-human）")
        })?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (body, total) = window(&mut stdout, from, limit)
        .map_err(|e| Error::new(format!("中身を読めません: {e}")))?;
    let status = child
        .wait()
        .map_err(|e| Error::new(format!("curlを待てません: {e}")))?;
    let errors = errors.join().unwrap_or_default();
    let (code, location) = errors
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(STATUS_MARK))
        .map(|rest| {
            let mut parts = rest.split_whitespace();
            let code = parts.next().and_then(|c| c.parse::<u16>().ok());
            let location = parts.next().map(str::to_string);
            (code, location)
        })
        .unwrap_or((None, None));
    let message: String = errors
        .lines()
        .filter(|line| !line.starts_with(STATUS_MARK) && !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if !status.success() {
        // curlの終了コード28はタイムアウト、63は大きすぎるとき
        let message = match status.code() {
            Some(28) => format!("{MAX_TIME_SECS}秒以内に応答がありません: {message}"),
            Some(63) => format!("{MAX_FILESIZE}バイトを超えるので、読みませんでした"),
            _ => format!("開けません: {message}"),
        };
        return Err(Error::new(message));
    }
    let status = code
        .filter(|c| *c != 0)
        .ok_or_else(|| Error::new("応答を読めません"))?;
    Ok(Page {
        status,
        location,
        body,
        total,
    })
}

/// 読み切るまで読み、`from` から `limit` バイトだけを残す。全体の量もあわせて返す。
fn window(reader: &mut impl Read, from: usize, limit: usize) -> std::io::Result<(Vec<u8>, usize)> {
    let mut kept = Vec::new();
    let mut total = 0;
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let start = total;
        total += n;
        // この塊のうち、[from, from + limit) に入る部分を残す
        let lo = from.max(start);
        let hi = (from + limit).min(total);
        if lo < hi {
            kept.extend_from_slice(&buf[lo - start..hi - start]);
        }
    }
    Ok((kept, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network() -> Vec<String> {
        vec!["docs.rs".to_string(), "*.github.com".to_string()]
    }

    #[test]
    fn only_listed_hosts_are_permitted() {
        let network = network();
        for url in [
            "https://docs.rs/serde",
            "https://DOCS.RS/serde?x=1#a",
            "https://docs.rs.:443/",
            "https://docs.rs",
            "https://api.github.com/repos/x/y",
            "https://raw.objects.github.com/",
            "HTTPS://api.GitHub.com",
        ] {
            assert!(permitted(url, &network).is_ok(), "{url}");
        }
        for url in [
            // ワイルドカードは、そのドメインそのものを含まない（sandboxと同じ）
            "https://github.com/",
            "https://evil-docs.rs/",
            "https://docs.rs.evil.com/",
            "https://evilgithub.com/",
            "https://example.com/?https://docs.rs/",
        ] {
            assert!(permitted(url, &network).is_err(), "{url}");
        }
    }

    #[test]
    fn only_https_on_the_default_port_is_opened() {
        let network = network();
        for url in [
            "http://docs.rs/",
            "ftp://docs.rs/",
            "file:///etc/passwd",
            "docs.rs",
            "https://docs.rs:8443/",
            "https://user@docs.rs/",
            "https://evil.com@docs.rs/",
            "https://docs.rs\\@evil.com/",
            "https://[::1]/",
            "https://127.0.0.1/",
            "https://docs%2ers/",
            "https://docs.rs/ x",
            "https://docs.rs/\n",
            "https://",
        ] {
            assert!(permitted(url, &network).is_err(), "{url}");
        }
    }

    #[test]
    fn refusals_say_what_to_ask_for_and_name_the_host_for_the_cli() {
        let err = permitted("https://example.com/a", &network()).unwrap_err();
        // CLIがclaudeの出力から拾う形
        assert!(
            err.message
                .contains("deny network-outbound example.com:443"),
            "{}",
            err.message
        );
        let hint = err.hint.unwrap();
        assert!(hint.contains("docs.rs、*.github.com"), "{hint}");
        assert!(hint.contains("needs-human"), "{hint}");
        assert!(hint.contains("ホストの名前は書かずに"), "{hint}");
        let hint = permitted("https://docs.rs/", &[])
            .unwrap_err()
            .hint
            .unwrap();
        assert!(hint.contains("開けるホストがありません"), "{hint}");
    }

    #[test]
    fn the_window_keeps_only_the_asked_part() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let (kept, total) = window(&mut data.as_slice(), 0, 100).unwrap();
        assert_eq!((kept.as_slice(), total), (&data[..100], 20_000));
        let (kept, _) = window(&mut data.as_slice(), 8190, 10_000).unwrap();
        assert_eq!(kept, &data[8190..18_190]);
        let (kept, _) = window(&mut data.as_slice(), 19_990, 100).unwrap();
        assert_eq!(kept, &data[19_990..]);
        let (kept, _) = window(&mut data.as_slice(), 30_000, 100).unwrap();
        assert!(kept.is_empty());
    }

    #[test]
    fn the_rest_is_announced_only_when_something_is_left() {
        assert_eq!(remaining("https://docs.rs/", 0, 10, 10), None);
        let note = remaining("https://docs.rs/", 0, 10, 25).unwrap();
        assert!(
            note.contains("agentcells cell fetch --from 10 https://docs.rs/"),
            "{note}"
        );
        assert_eq!(remaining("https://docs.rs/", 30, 0, 25), None);
    }

    /// 本物のcurlで、手元のサーバーから読む。
    #[cfg(unix)]
    mod curl {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        /// 1回だけリクエストを受けて、決まった応答を返すサーバー。cellの中で、手元のポートで待ち受けられなければ
        /// `None`（[`crate::github`] のテストと同じ）。
        fn serve_once(response: String) -> Option<(String, std::thread::JoinHandle<()>)> {
            let listener = match TcpListener::bind("127.0.0.1:0") {
                Ok(listener) => listener,
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied
                        && crate::run::inside_cell() =>
                {
                    eprintln!(
                        "cellのsandboxでは、手元のポートで待ち受けられないので、飛ばします: {e}"
                    );
                    return None;
                }
                Err(e) => panic!("{e}"),
            };
            let base = format!("http://{}", listener.local_addr().unwrap());
            let handle = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                stream.write_all(response.as_bytes()).unwrap();
            });
            Some((base, handle))
        }

        /// proxyの環境変数がある環境でも、手元のサーバーに直接送る。
        const NO_PROXY: &[(&str, &str)] = &[("NO_PROXY", "*"), ("no_proxy", "*")];

        #[test]
        fn reads_a_part_of_the_body() {
            let body = "x".repeat(50) + &"y".repeat(50);
            let Some((base, server)) = serve_once(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )) else {
                return;
            };
            let page = transfer(&format!("{base}/a"), "=http", 40, 20, NO_PROXY).unwrap();
            server.join().unwrap();
            assert_eq!(page.status, 200);
            assert_eq!(page.total, 100);
            assert_eq!(
                page.body,
                format!("{}{}", "x".repeat(10), "y".repeat(10)).as_bytes()
            );
        }

        #[test]
        fn reads_the_redirect_without_following_it() {
            let Some((base, server)) = serve_once(
                "HTTP/1.1 302 Found\r\nLocation: /next?a=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            ) else {
                return;
            };
            let page = transfer(&format!("{base}/a"), "=http", 0, LIMIT, NO_PROXY).unwrap();
            server.join().unwrap();
            assert_eq!(page.status, 302);
            assert_eq!(page.location, Some(format!("{base}/next?a=1")));
        }
    }
}
