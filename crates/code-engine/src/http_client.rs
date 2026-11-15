//! Prefer platform trust, while supporting hosts without an installed CA store.
pub(crate) fn http_client(
    make: impl Fn() -> reqwest::ClientBuilder,
) -> reqwest::Result<reqwest::Client> {
    match make().build() {
        Ok(client) => Ok(client),
        Err(_) => {
            let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
                .iter()
                .map(|cert| reqwest::Certificate::from_der(cert.as_ref()))
                .collect::<reqwest::Result<Vec<_>>>()?;
            make().tls_certs_only(roots).build()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::http_client;
    use std::cell::Cell;

    #[test]
    fn retries_with_bundled_roots_after_a_build_failure() {
        let attempts = Cell::new(0);
        let client = http_client(|| {
            attempts.set(attempts.get() + 1);
            let builder = reqwest::Client::builder().no_proxy();
            if attempts.get() == 1 {
                // An invalid header poisons this builder, independently of OS trust.
                builder.user_agent("\n")
            } else {
                builder
            }
        });
        assert!(client.is_ok());
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn successful_build_does_not_retry_and_failed_retry_is_returned() {
        let attempts = Cell::new(0);
        assert!(
            http_client(|| {
                attempts.set(attempts.get() + 1);
                reqwest::Client::builder().no_proxy().tls_certs_only([])
            })
            .is_ok()
        );
        assert_eq!(attempts.get(), 1);
        attempts.set(0);
        assert!(
            http_client(|| {
                attempts.set(attempts.get() + 1);
                reqwest::Client::builder().no_proxy().user_agent("\n")
            })
            .is_err()
        );
        assert_eq!(attempts.get(), 2);
    }
}
