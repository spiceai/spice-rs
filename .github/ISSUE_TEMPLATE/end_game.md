---
name: Milestone Endgame
about: Ship a milestone!
title: 'v0.x.x endgame'
labels: 'kind/endgame'
assignees: ''
---

## DRIs

|         | DRI |
| ------- | --- |
| Endgame |     |
| QA      |     |
| Docs    |     |

## Planning Checklist

- [ ] Review the specific [GitHub Milestone](https://github.com/spiceai/spice-rs/milestones)

## Release Checklist

- [ ] All features/bugfixes to be included in the release have been merged to trunk
- [ ] Full test pass and update if necessary over [README.md](https://github.com/spiceai/spice-rs/blob/trunk/README.md)
- [ ] Full test pass and update if necessary over Docs
  - [ ] [docs.spiceai.org](https://docs.spiceai.org/sdks/rust)
  - [ ] [docs.spice.ai](https://github.com/spicehq/docs/tree/trunk/sdks/rust-sdk)
- [ ] Test the [`spice-rs` sample](https://github.com/spiceai/samples/tree/trunk/client-sdk/spice-rs-sdk-sample) using the latest `trunk` SDK version.
- [ ] Update [release notes](https://github.com/spiceai/spice-rs/blob/trunk/docs/release_notes)
  - [ ] Ensure all contributors have been acknowledged.
- [ ] Verify `Cargo.toml` is set to the milestone version and the release tag will match it.
- [ ] Run [Test CI](https://github.com/spiceai/spice-rs/actions/workflows/build.yml) and ensure it is green on the trunk branch.
- [ ] QA DRI sign-off
- [ ] Docs DRI sign-off
- [ ] Create or publish the GitHub Release for the target version tag.
- [ ] If this should be the stable SDK release, mark the GitHub Release as Latest.
- [ ] Ensure the [publish](https://github.com/spiceai/spice-rs/actions/workflows/publish.yml) workflow completed successfully and published the package.
- [ ] Run a test pass using the [`spice-rs` sample](https://github.com/spiceai/samples/tree/trunk/client-sdk/spice-rs-sdk-sample) using the latest published version.
- [ ] The SDK release is added to the next [Spice release notes](https://github.com/spiceai/spiceai/tree/trunk/docs/release_notes)
