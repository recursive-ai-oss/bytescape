# Security policy

Bytescape reads file system structures that may be damaged or hostile, and it includes a small helper program that runs with administrator rights. Both matter for security, and we want to hear about problems in either.

## The security boundary: the elevated helper

The interface (`bytescape`) never runs with elevated rights. When a full scan or a delete that needs administrator rights calls for them, it starts `bytescape-helper`, a short-lived program that:

- accepts requests only from the interface that launched it,
- only reads, plus deletes of paths the interface lists explicitly,
- re-checks every delete against its own list of protected locations,
- never starts other programs, and
- exits when its work is done or its connection closes.

The helper is therefore the part of Bytescape where a bug is most serious. Its dependency list is deliberately short and reviewed.

Bytescape never connects to the internet, so there is no network service to attack. The interesting surface is local: the helper's request channel, and the parsers for raw disk data.

## Supported versions

Bytescape has not had a release yet. Once it does, security fixes go into the latest release only.

## Reporting a vulnerability

**Don't open a public issue, pull request or discussion.** Report it privately through GitHub's private vulnerability reporting:

1. Go to <https://github.com/recursive-ai-oss/bytescape/security/advisories/new> (the **Security** tab of the repository, then **Report a vulnerability**).
2. Describe the problem, the version and operating system, and the steps or the disk image that show it. The smallest reproduction helps most.

Only the maintainers can see the report. You can expect:

- an acknowledgement within 7 days;
- a fix, or a plan with a timeline, after we have confirmed the issue;
- a published security advisory once a fix is released, crediting you unless you ask us not to.

Please give us the chance to release a fix before you disclose the issue publicly.

## What counts

Examples of what we treat as security issues:

- any way to make the helper act for a program other than the interface that launched it;
- a delete through the helper that skips the protected-location checks;
- the helper starting another program;
- a crash, hang, or unbounded memory use in a disk-format reader when given a damaged or crafted file system;
- any network connection made by any Bytescape program;
- a way to run code with the helper's rights through its installed location, its request channel or its files.

Wrong sizes or a map that draws incorrectly, with no security effect, are ordinary bugs. Please open a regular issue for those.

## A note on portable copies

The portable build runs from a folder you choose. Wherever you place it, the helper can be replaced by anyone who can write to that folder, so keep the portable copy in a location only you control. The installed version keeps the helper in a folder that only administrators can write to.
