#[derive(Debug, Clone, serde::Serialize)]
pub struct TestTarget {
    pub key: &'static str,
    pub label: &'static str,
    pub host: &'static str,
    pub trace_url: &'static str,
    pub download_url: &'static str,
}

pub const TEST_TARGETS: &[TestTarget] = &[
    TestTarget {
        key: "a1",
        label: "a1",
        host: "a1.steinsgate.eu.org",
        trace_url: "https://a1.steinsgate.eu.org/cdn-cgi/trace",
        download_url: "https://a1.steinsgate.eu.org/200mb.test",
    },
    TestTarget {
        key: "a2",
        label: "a2",
        host: "a2.steinsgate.eu.org",
        trace_url: "https://a2.steinsgate.eu.org/cdn-cgi/trace",
        download_url: "https://a2.steinsgate.eu.org/200mb.test",
    },
];

pub fn find_target(key: &str) -> Option<&'static TestTarget> {
    TEST_TARGETS.iter().find(|target| target.key == key)
}
