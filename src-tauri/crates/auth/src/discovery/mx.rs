//! MX lookup used as a last-resort provider hint for custom domains.

use std::time::Duration;

use hickory_resolver::proto::rr::{RData, Record, RecordType};
use hickory_resolver::Resolver;

/// Resolve MX hosts for a domain, most preferred first.
///
/// Network failures and invalid domains yield an empty list: discovery then
/// falls through to the generic `imap.<domain>` guess.
pub async fn lookup_mx(domain: &str) -> Vec<String> {
    let domain = domain.trim().trim_end_matches('.');
    if domain.is_empty() {
        return Vec::new();
    }

    let resolver = match Resolver::builder_tokio().and_then(|builder| builder.build()) {
        Ok(resolver) => resolver,
        Err(error) => {
            tracing::debug!(%error, "cannot create DNS resolver for MX discovery");
            return Vec::new();
        }
    };

    match tokio::time::timeout(
        Duration::from_secs(6),
        resolver.lookup(domain.to_string(), RecordType::MX),
    )
    .await
    {
        Ok(Ok(lookup)) => mx_hosts_from_answers(lookup.answers()),
        Ok(Err(error)) => {
            tracing::debug!(%error, domain, "MX lookup failed");
            Vec::new()
        }
        Err(_) => {
            tracing::debug!(domain, "MX lookup timed out");
            Vec::new()
        }
    }
}

fn mx_hosts_from_answers(answers: &[Record]) -> Vec<String> {
    let mut entries: Vec<(u16, String)> = answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::MX(mx) => Some((
                mx.preference,
                mx.exchange
                    .to_utf8()
                    .trim_end_matches('.')
                    .to_ascii_lowercase(),
            )),
            _ => None,
        })
        .filter(|(_, host)| !host.is_empty())
        .collect();
    entries.sort_by_key(|(preference, _)| *preference);
    entries.into_iter().map(|(_, host)| host).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_resolver::proto::rr::rdata::MX;
    use hickory_resolver::proto::rr::{Name, RData};

    fn mx_record(host: &str, preference: u16) -> Record {
        let name = Name::from_ascii("example.com.").expect("name");
        let exchange = Name::from_ascii(format!("{host}.")).expect("exchange");
        Record::from_rdata(name, 300, RData::MX(MX::new(preference, exchange)))
    }

    #[test]
    fn extracts_and_sorts_mx_hosts() {
        let answers = vec![
            mx_record("mx2.example.com", 20),
            mx_record("mx1.example.com", 10),
        ];
        assert_eq!(
            mx_hosts_from_answers(&answers),
            vec!["mx1.example.com".to_string(), "mx2.example.com".to_string()]
        );
    }

    #[test]
    fn ignores_non_mx_records() {
        let name = Name::from_ascii("example.com.").expect("name");
        let answer = Record::from_rdata(name, 300, RData::A("192.0.2.1".parse().expect("ip")));
        assert!(mx_hosts_from_answers(&[answer]).is_empty());
    }

    #[test]
    fn empty_domain_returns_nothing_without_dns() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        assert!(runtime.block_on(lookup_mx("  ")).is_empty());
    }

    #[test]
    #[ignore = "requires network access"]
    fn resolves_gmail_mx() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let hosts = runtime.block_on(lookup_mx("gmail.com"));
        assert!(!hosts.is_empty());
        assert!(hosts.iter().any(|host| host.contains("google.com")));
    }
}
