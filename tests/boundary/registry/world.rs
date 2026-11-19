//! The registry conformance world backed by a real registry on loopback
//! (the run's `local-registry`). Images are published with the OCI
//! distribution API; awman reaches the registry through a counting
//! forwarder so the suite can tell whether a request was made.
//!
//! Published repositories are left in the job's registry (it is the job's
//! own throwaway container); every image the suite publishes is
//! content-addressed, so a second run is idempotent.

use super::client::{request, Reply};
use super::proxy::CountingProxy;
use awman::data::config::env::EnvSnapshot;
use awman::data::config::image_source::RegistryHostConfig;
use awman::engine::conformance::registry::{
    PublishedImage, RegistryWorld, FOREIGN_TAG, HOST_TAG, REPOSITORY,
};
use awman_test_support::fixtures::{OciImage, OciImageBuilder};
use awman_test_support::TestSandbox;
use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::support;

pub struct RealRegistryWorld<'a> {
    sandbox: &'a TestSandbox,
    upstream: SocketAddr,
    proxy: RefCell<CountingProxy>,
    states: Cell<usize>,
    #[cfg(unix)]
    tls: Option<crate::docker::tls::Endpoint>,
}

impl<'a> RealRegistryWorld<'a> {
    pub fn new(sandbox: &'a TestSandbox, upstream: SocketAddr) -> RealRegistryWorld<'a> {
        let proxy = CountingProxy::start(sandbox.ports().tcp_listener(), upstream);
        RealRegistryWorld {
            sandbox,
            upstream,
            proxy: RefCell::new(proxy),
            states: Cell::new(0),
            #[cfg(unix)]
            tls: None,
        }
    }

    #[cfg(unix)]
    pub fn with_tls(mut self) -> Self {
        use base64::Engine as _;
        let authorization = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode("awman-test-user:test-secret-registry-password")
        );
        let address = self.proxy.borrow().addr();
        self.tls = Some(crate::docker::tls::Endpoint::start_tcp(
            self.sandbox,
            address,
            Some(authorization),
        ));
        self
    }

    fn call(&self, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        request(self.upstream, method, target, headers, body)
            .unwrap_or_else(|e| panic!("registry {} {target}: {e}", self.upstream))
    }

    fn upload_blob(&self, digest: &str, bytes: &[u8]) {
        let started = self.call(
            "POST",
            &format!("/v2/{REPOSITORY}/blobs/uploads/"),
            &[],
            b"",
        );
        assert_eq!(started.status, 202, "starting a blob upload");
        let location = started
            .header("location")
            .expect("upload Location")
            .to_string();
        let path = match location.split_once("://") {
            Some((_, rest)) => rest
                .find('/')
                .map(|i| rest[i..].to_string())
                .unwrap_or_default(),
            None => location,
        };
        let sep = if path.contains('?') { '&' } else { '?' };
        let done = self.call(
            "PUT",
            &format!("{path}{sep}digest={digest}"),
            &[("Content-Type", "application/octet-stream")],
            bytes,
        );
        assert_eq!(
            done.status, 201,
            "finishing the upload of {digest}: {:?}",
            done.body
        );
    }

    fn publish(&self, tag: &str, image: &OciImage) {
        for blob in image.layers.iter().chain([&image.config]) {
            self.upload_blob(&blob.digest, &blob.bytes);
        }
        let put = self.call(
            "PUT",
            &format!("/v2/{REPOSITORY}/manifests/{tag}"),
            &[("Content-Type", image.manifest.media_type.as_str())],
            &image.manifest.bytes,
        );
        assert_eq!(put.status, 201, "publishing {REPOSITORY}:{tag}");
    }
}

impl RegistryWorld for RealRegistryWorld<'_> {
    fn subject(&self) -> String {
        format!("OCI registry (real, {})", self.upstream)
    }

    fn env(&self) -> EnvSnapshot {
        support::env_snapshot(self.sandbox)
    }

    fn registry(&self) -> String {
        #[cfg(unix)]
        if let Some(tls) = &self.tls {
            return tls.host.strip_prefix("tcp://").unwrap().to_string();
        }
        self.proxy.borrow().addr().to_string()
    }

    fn host_config(&self) -> RegistryHostConfig {
        #[cfg(unix)]
        if let Some(tls) = &self.tls {
            return RegistryHostConfig {
                insecure: false,
                ca_cert: Some(tls.tls.ca.clone()),
                auth: Some(awman::data::config::image_source::RegistryAuthSource::Env {
                    username_var: "AWMAN_REGISTRY_TEST_USER".into(),
                    password_var: "AWMAN_REGISTRY_TEST_PASSWORD".into(),
                }),
            };
        }
        RegistryHostConfig {
            insecure: true,
            ca_cert: None,
            auth: None,
        }
    }

    fn state_dir(&self) -> PathBuf {
        let n = self.states.get() + 1;
        self.states.set(n);
        let dir = self.sandbox.root().join(format!("registry-state-{n}"));
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("BLOCKED[tempdir]: {}: {e}", dir.display()));
        dir
    }

    fn given_host_image(&self) -> PublishedImage {
        let image = OciImageBuilder::new()
            .layer(&[("conformance/host", b"awman registry conformance\n")])
            .gzip(true)
            .build();
        self.publish(HOST_TAG, &image);
        PublishedImage {
            manifest_digest: image.manifest.digest.clone(),
            config_digest: image.config.digest.clone(),
        }
    }

    fn given_foreign_image(&self) {
        let image = OciImageBuilder::new()
            .architecture("s390x")
            .layer(&[("conformance/foreign", b"another platform\n")])
            .gzip(true)
            .build();
        self.publish(FOREIGN_TAG, &image);
    }

    fn requests_served(&self) -> usize {
        self.proxy.borrow().accepted()
    }

    fn cleanup(&self) {
        self.proxy.borrow_mut().stop();
    }
}
