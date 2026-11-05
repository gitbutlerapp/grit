//! Public coverage for [`grit_lib::remote::redact_url_for_display`]: the display form of a
//! remote address drops credentials and changes nothing else.

use grit_lib::remote::redact_url_for_display;

#[test]
fn credentials_are_removed_from_every_url_form() {
    let cases = [
        (
            "http://alice:s3cret@127.0.0.1:9419/auth.git",
            "http://127.0.0.1:9419/auth.git",
        ),
        (
            "https://x-access-token:tok@Host.Example:8443/org/repo@v1.git",
            "https://Host.Example:8443/org/repo@v1.git",
        ),
        (
            "ssh://git@host.example:2222/~/repo.git",
            "ssh://host.example:2222/~/repo.git",
        ),
        ("git@host.example:org/repo.git", "host.example:org/repo.git"),
        (
            "  http://u:p%40ss@host.example/r.git\n",
            "http://host.example/r.git",
        ),
    ];
    for (url, shown) in cases {
        assert_eq!(redact_url_for_display(url), shown, "{url:?}");
    }
}

#[test]
fn addresses_without_credentials_are_returned_as_given() {
    for url in [
        "origin",
        "http://127.0.0.1:9419/auth.git",
        "HTTPS://Host.Example:443/A%2Fb.git",
        "file:///srv/me@work/repo.git",
        "/srv/alice@laptop/repo.git",
        "../me@there",
        "host.example:repo@v1.git",
    ] {
        assert_eq!(redact_url_for_display(url), url);
    }
}
