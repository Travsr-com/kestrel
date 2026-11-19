//! Provider keys never show up in `Debug` output.

use kestrel_gateway::ProviderConfig;
use url::Url;

#[test]
fn debug_hides_the_api_key() {
    let p = ProviderConfig {
        base_url: Url::parse("https://api.anthropic.com").expect("url"),
        api_key: "sk-ant-very-secret".into(),
    };
    let text = format!("{p:?}");
    assert!(!text.contains("very-secret"), "{text}");
    assert!(text.contains("***"), "{text}");
    assert!(text.contains("api.anthropic.com"), "{text}");
}
