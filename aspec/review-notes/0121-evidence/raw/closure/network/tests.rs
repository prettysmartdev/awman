    #[tokio::test]
    async fn strict_sni_end_to_end_tls_reaches_allowed_server_without_interception() {
        use rustls::pki_types::PrivatePkcs8KeyDer;
        let certified = rcgen::generate_simple_self_signed(vec!["allowed.example".into()]).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions().unwrap().with_root_certificates(roots)
            .with_no_client_auth();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dst = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(server)).accept(stream).await.unwrap();
            let mut request = [0; 4];
            tls.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            tls.write_all(b"pong").await.unwrap();
            tls.shutdown().await.unwrap();
        });
        let shared = Arc::new(shared_with("allowed.example", "127.0.0.1"));
        let policy = Arc::new(NetworkPolicy {
            default_egress: Action::Deny, default_ingress: Action::Deny,
            rules: vec![allow_tcp("allowed.example", dst.port())],
        });
        let (from_tx, from_rx) = mpsc::channel(8);
        let (to_tx, mut to_rx) = mpsc::channel::<Bytes>(8);
        let status = Arc::new(ProxyConnectState::new());
        let proxy = TcpProxy::new(dst, UpstreamTcpTarget::direct(dst), from_rx, to_tx,
            shared, policy, Arc::new(SecretsConfig::default()), None, true, status.clone(), None)
            .with_strict_sni(true);
        let proxy_task = tokio::spawn(proxy.try_run());
        let (guest, channel) = tokio::io::duplex(16384);
        let (mut read, mut write) = tokio::io::split(channel);
        let outbound = tokio::spawn(async move {
            let mut buf = [0; 8192];
            while let Ok(n) = read.read(&mut buf).await {
                if n == 0 || from_tx.send(Bytes::copy_from_slice(&buf[..n])).await.is_err() { break; }
            }
        });
        let inbound = tokio::spawn(async move {
            while let Some(bytes) = to_rx.recv().await {
                if write.write_all(&bytes).await.is_err() { break; }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut tls = tokio_rustls::TlsConnector::from(Arc::new(client))
                .connect("allowed.example".try_into().unwrap(), guest).await.unwrap();
            tls.write_all(b"ping").await.unwrap();
            let mut response = [0; 4];
            tls.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"pong");
            tls.shutdown().await.unwrap();
            server_task.await.unwrap();
            proxy_task.await.unwrap().unwrap();
        }).await.expect("allowed TLS handshake and exchange must finish");
        assert_ne!(status.status(), ProxyConnectStatus::PolicyDenied);
        outbound.abort(); inbound.abort();
    }

    #[tokio::test]
    async fn strict_sni_denies_missing_wrong_unbound_and_sibling_names_before_dial() {
        for (label, bytes, cached_name, suffix) in [
            ("missing", vec![0x16, 0x03, 0x01], "allowed.example", false),
            ("wrong shared-IP name", synthetic_client_hello("unlisted.example"), "allowed.example", false),
            ("missing DNS binding", synthetic_client_hello("allowed.example"), "other.example", false),
            ("sibling DNS binding", synthetic_client_hello("b.example"), "a.example", true),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let dst = listener.local_addr().unwrap();
            let shared = Arc::new(shared_with(cached_name, "127.0.0.1"));
            let mut rule = allow_tcp("allowed.example", dst.port());
            if suffix { rule.destination = Destination::DomainSuffix(".example".parse().unwrap()); }
            let policy = Arc::new(NetworkPolicy { default_egress: Action::Deny,
                default_ingress: Action::Deny, rules: vec![rule] });
            let (from_tx, from_rx) = mpsc::channel(8);
            let (to_tx, _to_rx) = mpsc::channel(8);
            from_tx.send(Bytes::from(bytes)).await.unwrap();
            drop(from_tx);
            let status = Arc::new(ProxyConnectState::new());
            TcpProxy::new(dst, UpstreamTcpTarget::direct(dst), from_rx, to_tx,
                shared, policy, Arc::new(SecretsConfig::default()), None, true, status.clone(), None)
                .with_strict_sni(true).try_run().await.unwrap();
            assert_eq!(status.status(), ProxyConnectStatus::PolicyDenied, "{label}");
            assert!(tokio::time::timeout(Duration::from_millis(20), listener.accept()).await.is_err(),
                "{label}: denied connection must never dial upstream");
        }
    }

