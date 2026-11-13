use maya_nmh::{
    generate_chrome_manifest, generate_firefox_manifest, ChromeManifest, FirefoxManifest,
    DEFAULT_CHROME_EXTENSION_ID, FIREFOX_EXTENSION_ID, HOST_NAME,
};
use std::path::Path;

#[test]
fn test_chrome_manifest_structure() {
    let host = Path::new("C:/Program Files/Maya/maya-nmh.exe");
    let origins = vec![
        "chrome-extension://abcdefghijklmnop/".to_string(),
        "chrome-extension://qrstuvwxyz/".to_string(),
    ];
    let manifest = generate_chrome_manifest(host, &origins);

    assert_eq!(manifest.name, HOST_NAME);
    assert_eq!(manifest.r#type, "stdio");
    assert_eq!(manifest.path, "C:/Program Files/Maya/maya-nmh.exe");
    assert_eq!(manifest.allowed_origins.len(), 2);
    assert_eq!(manifest.allowed_origins[0], origins[0]);

    let json_val: serde_json::Value = serde_json::to_value(&manifest).expect("serialize to value");
    assert_eq!(json_val["type"], "stdio");
    assert_eq!(json_val["name"], "com.maya.downloader");
}

#[test]
fn test_chrome_manifest_default_origin() {
    let host = Path::new("C:/Program Files/Maya/maya-nmh.exe");
    let manifest = generate_chrome_manifest(host, &[]);
    assert_eq!(manifest.allowed_origins, vec![DEFAULT_CHROME_EXTENSION_ID]);
    assert!(!manifest
        .allowed_origins
        .contains(&"chrome-extension://*".to_string()));
}

#[test]
fn test_firefox_manifest_structure() {
    let host = Path::new("/usr/bin/maya-nmh");
    let extensions = vec!["custom@extension.id".to_string()];
    let manifest = generate_firefox_manifest(host, &extensions);

    assert_eq!(manifest.name, HOST_NAME);
    assert_eq!(manifest.r#type, "stdio");
    assert_eq!(manifest.path, "/usr/bin/maya-nmh");
    assert_eq!(manifest.allowed_extensions, vec!["custom@extension.id"]);

    let json_val: serde_json::Value = serde_json::to_value(&manifest).expect("serialize to value");
    assert_eq!(json_val["type"], "stdio");
    assert_eq!(json_val["name"], "com.maya.downloader");
    assert!(json_val["allowed_extensions"].is_array());
}

#[test]
fn test_firefox_manifest_default_extension() {
    let host = Path::new("/usr/bin/maya-nmh");
    let manifest = generate_firefox_manifest(host, &[]);
    assert_eq!(manifest.allowed_extensions, vec![FIREFOX_EXTENSION_ID]);
}

#[test]
fn test_manifest_deserialization() {
    let raw = r#"{
        "name": "com.maya.downloader",
        "description": "Test host",
        "path": "C:\\test\\bin.exe",
        "type": "stdio",
        "allowed_origins": ["chrome-extension://test/"]
    }"#;

    let parsed: ChromeManifest = serde_json::from_str(raw).expect("parse chrome manifest");
    assert_eq!(parsed.name, "com.maya.downloader");
    assert_eq!(parsed.allowed_origins, vec!["chrome-extension://test/"]);

    let raw_ff = r#"{
        "name": "com.maya.downloader",
        "description": "Test host",
        "path": "/opt/bin",
        "type": "stdio",
        "allowed_extensions": ["ext@maya"]
    }"#;

    let parsed_ff: FirefoxManifest = serde_json::from_str(raw_ff).expect("parse ff manifest");
    assert_eq!(parsed_ff.name, "com.maya.downloader");
    assert_eq!(parsed_ff.allowed_extensions, vec!["ext@maya"]);
}
