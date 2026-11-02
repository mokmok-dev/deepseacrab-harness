//! SSH hosts parsed from the environment.

use crate::error::SshError;
use dsh_core::Env;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Environment variable holding the JSON host list.
pub const HOSTS_ENV: &str = "DSH_SSH_HOSTS";

/// SSH client used unless the plugin names another executable.
pub const DEFAULT_SSH: &str = "ssh";

/// One remote host the `ssh_run` tool may reach.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHost {
    /// Stable label the model passes as `host`.
    pub name: String,
    /// Hostname or address handed to `ssh`.
    pub host: String,
    /// Remote user, when it differs from the local user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// TCP port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Private key file passed with `-i`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PathBuf>,
}

/// Parse a JSON array of host objects.
///
/// Only `name` and `host` are required. Names must be non-empty and free of
/// whitespace, hosts must not be blank, names must be unique, and a port must
/// be in `1..=65535`.
pub fn parse_hosts(json: &str) -> Result<Vec<SshHost>, SshError> {
    let hosts: Vec<SshHost> =
        serde_json::from_str(json).map_err(|error| SshError::Config(error.to_string()))?;
    validate(&hosts)?;
    Ok(hosts)
}

fn validate(hosts: &[SshHost]) -> Result<(), SshError> {
    let mut names = BTreeSet::new();
    for host in hosts {
        if host.name.is_empty() {
            return Err(SshError::Config("host name must not be empty".to_string()));
        }
        if host.name.chars().any(char::is_whitespace) {
            return Err(SshError::Config(format!(
                "host name {:?} must not contain whitespace",
                host.name
            )));
        }
        if host.host.trim().is_empty() {
            return Err(SshError::Config(format!(
                "host {} must name a host",
                host.name
            )));
        }
        if host.port == Some(0) {
            return Err(SshError::Config(format!(
                "host {} port must be in 1..=65535",
                host.name
            )));
        }
        if !names.insert(host.name.as_str()) {
            return Err(SshError::Config(format!(
                "host name {:?} is configured twice",
                host.name
            )));
        }
    }
    Ok(())
}

/// Hosts configured through [`HOSTS_ENV`].
///
/// An absent or blank value yields no hosts. A malformed value is reported so
/// the plugin can refuse composition instead of silently dropping hosts.
pub fn hosts_from_env(env: &dyn Env) -> Result<Vec<SshHost>, SshError> {
    let Some(raw) = env.var(HOSTS_ENV) else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_hosts(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn with(key: &str, value: &str) -> Self {
            Self(HashMap::from([(key.to_string(), value.to_string())]))
        }
    }

    impl Env for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn parse(value: &serde_json::Value) -> Result<Vec<SshHost>, SshError> {
        parse_hosts(&value.to_string())
    }

    fn detail(result: Result<Vec<SshHost>, SshError>) -> String {
        match result {
            Err(SshError::Config(detail)) => detail,
            Ok(hosts) => panic!("expected a configuration error, parsed {hosts:?}"),
        }
    }

    fn web() -> SshHost {
        SshHost {
            name: "web".to_string(),
            host: "example.com".to_string(),
            user: None,
            port: None,
            identity: None,
        }
    }

    #[test]
    fn every_documented_field_reaches_the_typed_host() {
        let hosts = parse(&json!([{
            "name": "web",
            "host": "example.com",
            "user": "alice",
            "port": 2222,
            "identity": "/home/alice/.ssh/id_ed25519"
        }]))
        .expect("parses");
        assert_eq!(
            hosts,
            vec![SshHost {
                name: "web".to_string(),
                host: "example.com".to_string(),
                user: Some("alice".to_string()),
                port: Some(2222),
                identity: Some(PathBuf::from("/home/alice/.ssh/id_ed25519")),
            }]
        );
    }

    #[test]
    fn optional_fields_are_absent_by_default() {
        assert_eq!(
            parse(&json!([{"name": "web", "host": "example.com"}])).expect("parses"),
            vec![web()]
        );
    }

    #[test]
    fn malformed_documents_are_rejected() {
        for raw in ["", "{", "{}", "null", "7", r#"[{"name": 7, "host": "x"}]"#] {
            assert!(
                matches!(parse_hosts(raw), Err(SshError::Config(_))),
                "document {raw:?} was accepted"
            );
        }
    }

    #[test]
    fn empty_names_are_rejected() {
        assert_eq!(
            detail(parse(&json!([{"name": "", "host": "example.com"}]))),
            "host name must not be empty"
        );
    }

    #[test]
    fn names_with_whitespace_are_rejected() {
        for name in [" ", "web server", "web\tserver", "web\n"] {
            assert!(
                detail(parse(&json!([{"name": name, "host": "example.com"}])))
                    .contains("must not contain whitespace"),
                "name {name:?} was accepted"
            );
        }
    }

    #[test]
    fn blank_hosts_are_rejected() {
        for host in ["", " ", "\t"] {
            assert!(
                detail(parse(&json!([{"name": "web", "host": host}]))).contains("must name a host"),
                "host {host:?} was accepted"
            );
        }
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let document = json!([
            {"name": "web", "host": "a.example.com"},
            {"name": "web", "host": "b.example.com"}
        ]);
        assert!(detail(parse(&document)).contains("configured twice"));
    }

    #[test]
    fn distinct_names_are_accepted() {
        let hosts = parse(&json!([
            {"name": "web", "host": "a.example.com"},
            {"name": "db", "host": "b.example.com"}
        ]))
        .expect("parses");
        assert_eq!(hosts.len(), 2);
    }

    #[test]
    fn ports_outside_the_range_are_rejected() {
        assert!(detail(parse(&json!([{"name": "web", "host": "x", "port": 0}]))).contains("port"));
        for port in [65_536, 70_000] {
            assert!(
                matches!(
                    parse(&json!([{"name": "web", "host": "x", "port": port}])),
                    Err(SshError::Config(_))
                ),
                "port {port} was accepted"
            );
        }
    }

    #[test]
    fn the_port_boundaries_are_accepted() {
        for port in [1, 65_535] {
            let hosts =
                parse(&json!([{"name": "web", "host": "x", "port": port}])).expect("parses");
            assert_eq!(hosts[0].port, Some(port));
        }
    }

    #[test]
    fn an_absent_or_blank_variable_yields_no_hosts() {
        assert_eq!(hosts_from_env(&MapEnv::default()), Ok(Vec::new()));
        assert_eq!(hosts_from_env(&MapEnv::with(HOSTS_ENV, "")), Ok(Vec::new()));
        assert_eq!(
            hosts_from_env(&MapEnv::with(HOSTS_ENV, "  \n ")),
            Ok(Vec::new())
        );
        assert_eq!(
            hosts_from_env(&MapEnv::with("OTHER", "garbage")),
            Ok(Vec::new())
        );
    }

    #[test]
    fn the_variable_value_is_parsed() {
        let hosts = hosts_from_env(&MapEnv::with(
            HOSTS_ENV,
            r#"[{"name":"web","host":"example.com"}]"#,
        ))
        .expect("parses");
        assert_eq!(hosts, vec![web()]);
    }

    #[test]
    fn a_malformed_variable_value_is_reported() {
        assert!(matches!(
            hosts_from_env(&MapEnv::with(HOSTS_ENV, "{")),
            Err(SshError::Config(_))
        ));
    }

    proptest! {
        #[test]
        fn host_name_validation_matches_the_rule(name in "[ -~]{0,8}") {
            let result = parse(&json!([{"name": name, "host": "example.com"}]));
            let expected = !name.is_empty() && !name.chars().any(char::is_whitespace);
            prop_assert_eq!(result.is_ok(), expected);
        }

        #[test]
        fn host_documents_round_trip(hosts in prop::collection::vec(any::<SshHost>(), 0..4).prop_map(|hosts| {
            hosts
                .into_iter()
                .enumerate()
                .map(|(index, mut host)| {
                    host.name = format!("{}-{index}", host.name);
                    host
                })
                .collect::<Vec<SshHost>>()
        })) {
            let document = serde_json::to_string(&hosts).expect("serializes");
            prop_assert_eq!(parse_hosts(&document).expect("parses"), hosts);
        }
    }

    impl Arbitrary for SshHost {
        type Parameters = ();
        type Strategy = BoxedStrategy<Self>;

        fn arbitrary_with(_parameters: ()) -> Self::Strategy {
            (
                "[A-Za-z0-9_-]{1,8}",
                "[A-Za-z0-9.-]{1,12}",
                prop::option::of("[a-z]{1,8}"),
                prop::option::of(1u16..=65_535),
                prop::option::of("[A-Za-z0-9/._-]{1,16}"),
            )
                .prop_map(|(name, host, user, port, identity)| Self {
                    name,
                    host,
                    user,
                    port,
                    identity: identity.map(PathBuf::from),
                })
                .boxed()
        }
    }
}
