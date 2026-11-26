use super::*;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn listener(check: impl FnOnce(TcpStream) + Send + 'static) -> (String, JoinHandle<()>) {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap().to_string();
    socket.set_nonblocking(true).unwrap();
    let handler = thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(6);
        let stream = loop {
            match socket.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                    thread::sleep(Duration::from_millis(5));
                }
                result => panic!("proxy fixture did not accept client: {result:?}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        check(stream);
    });
    (address, handler)
}

fn headers(stream: &mut BufReader<TcpStream>) -> String {
    let mut result = String::new();
    loop {
        let mut line = String::new();
        assert!(stream.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        result.push_str(&line);
        assert!(result.len() < 8192);
    }
    result
}

#[test]
fn http_connect_sends_proxy_credentials_and_starts_tls_in_the_tunnel() {
    let (address, handler) = listener(|stream| {
        let mut stream = BufReader::new(stream);
        let head = headers(&mut stream);
        assert!(
            head.starts_with("CONNECT target.invalid:443 HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("proxy-authorization: basic dxnlcjpwyxnz")
        );
        stream
            .get_mut()
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .unwrap();
        let mut record = [0; 6];
        stream.read_exact(&mut record).unwrap();
        assert_eq!(record[0], 22); // TLS handshake record
        assert_eq!(record[5], 1); // ClientHello
        // Explicitly stop after testing tunnel establishment, before certificate verification.
        stream
            .get_mut()
            .write_all(&[21, 3, 3, 0, 2, 2, 40])
            .unwrap();
    });
    let cfg = ProxyConfig::custom(&format!("http://user:pass@{address}")).unwrap();
    let client = cfg
        .blocking_builder()
        .unwrap()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    assert!(client.get("https://target.invalid/").send().is_err());
    handler.join().unwrap();
}

fn bytes<const N: usize>(stream: &mut TcpStream) -> [u8; N] {
    let mut buf = [0; N];
    stream.read_exact(&mut buf).unwrap();
    buf
}

#[test]
fn socks5_local_resolution_and_socks5h_remote_resolution_reach_http() {
    for remote_dns in [false, true] {
        let (address, handler) = listener(move |mut stream| {
            let [version, count] = bytes(&mut stream);
            assert_eq!(version, 5);
            let mut methods = vec![0; count as usize];
            stream.read_exact(&mut methods).unwrap();
            assert!(methods.contains(&2));
            stream.write_all(&[5, 2]).unwrap();
            assert_eq!(bytes::<2>(&mut stream), [1, 4]);
            assert_eq!(bytes::<4>(&mut stream), *b"user");
            assert_eq!(bytes::<1>(&mut stream), [4]);
            assert_eq!(bytes::<4>(&mut stream), *b"pass");
            stream.write_all(&[1, 0]).unwrap();
            let [version, command, reserved, kind] = bytes(&mut stream);
            assert_eq!([version, command, reserved], [5, 1, 0]);
            if remote_dns {
                assert_eq!(kind, 3);
                let [length] = bytes(&mut stream);
                let mut host = vec![0; length as usize];
                stream.read_exact(&mut host).unwrap();
                assert_eq!(host, b"target.invalid");
            } else {
                assert_eq!(kind, 1);
                assert_eq!(bytes::<4>(&mut stream), [127, 0, 0, 1]);
            }
            assert_eq!(u16::from_be_bytes(bytes(&mut stream)), 8123);
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .unwrap();
            let mut stream = BufReader::new(stream);
            let head = headers(&mut stream);
            assert!(head.starts_with("GET /readme HTTP/1.1\r\n"));
            assert!(!head.to_ascii_lowercase().contains("proxy-authorization"));
            stream
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let (scheme, host) = if remote_dns {
            ("socks5h", "target.invalid")
        } else {
            ("socks5", "127.0.0.1")
        };
        let config = ProxyConfig::custom(&format!("{scheme}://user:pass@{address}")).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let body = runtime
            .block_on(async {
                config
                    .async_builder()?
                    .timeout(Duration::from_secs(4))
                    .build()?
                    .get(format!("http://{host}:8123/readme"))
                    .send()
                    .await?
                    .text()
                    .await
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(body, "ok");
        handler.join().unwrap();
    }
}

#[test]
fn shared_configuration_is_redacted_and_requires_current_admin() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut primary = store::connect_as(dir.path(), store::PRIMARY)?;
    primary.batch_execute("INSERT INTO accounts VALUES ('primary','admin','fixture',1,0,2),('reader','reader','fixture',0,0,1)")?;
    let mut reader = store::connect_as(dir.path(), "reader")?;
    let update = json!({"mode":"custom","url":"socks5h://fixture:never-echo@[::1]:1080"});
    assert!(read_settings(&mut reader).is_err());
    assert!(persist(&mut reader, update.clone(), 1, None).is_err());
    assert!(persist(&mut primary, update.clone(), 1, None).is_err());
    let status = persist(&mut primary, update, 2, None)?;
    assert_eq!(status["address"], "socks5h://[::1]:1080");
    assert_eq!(status["has_credentials"], true);
    assert!(!status.to_string().contains("never-echo"));
    let (selected, source) = effective(&mut reader, None)?;
    assert_eq!(selected.status(source), status);
    let (overridden, source) = effective(
        &mut reader,
        Some("https://ops:secret@proxy.invalid:8443".into()),
    )?;
    assert_eq!(
        overridden.status(source),
        json!({"source":"environment","address":"https://proxy.invalid:8443","has_credentials":true,"mode":"custom"})
    );
    assert!(
        persist(
            &mut primary,
            json!({"mode":"direct"}),
            2,
            Some("http://proxy.invalid".into())
        )
        .is_err()
    );
    for bad in [
        "file:///private",
        "socks4://x:1",
        "http://x:bogus",
        "https://x/path?q=never-echo",
    ] {
        let error = persist(&mut primary, json!({"mode":"custom","url":bad}), 2, None).unwrap_err();
        assert!(!format!("{error:#}").contains(bad));
    }
    assert_eq!(
        persist(&mut primary, json!({"mode":"direct"}), 2, None)?["mode"],
        "direct"
    );
    assert_eq!(
        persist(&mut primary, json!({"mode":"system"}), 2, None)?["source"],
        "system"
    );
    Ok(())
}
