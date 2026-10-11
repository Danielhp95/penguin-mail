#!/bin/sh
# Pulls the mail and calendar servers the Docker-backed tests start, each
# pinned by digest. The tests never pull: without an image they skip, or
# fail under PENGUIN_MAIL_REQUIRE_IMAP or PENGUIN_MAIL_REQUIRE_JMAP. Run
# this once on a new computer and after a digest changes in testmail/src.
set -eu
for image in \
    "dovecot/dovecot:2.4.5@sha256:c807be4fb5a97d9c3a90770569d3a6c4cbdcb36742ad41f90409cbd929166553" \
    "axllent/mailpit:v1.31.2@sha256:74d609a42ec279aa63c6b4622a6fa9b5408d1ad5b1d76a1c4be40a265ce0863d" \
    "ghcr.io/kozea/radicale:3.8.1@sha256:54d9406cd30f9e206a9dd6d61dfac48da26b9afc37200e14f23beb8fafa7d11c" \
    "ghcr.io/cyrusimap/cyrus-docker-test-server:bookworm@sha256:035aa4ffdb84df95a1057645d05c9a156dde74e5787ef1a11f7d9c244a38b2c0" \
    "stalwartlabs/stalwart:v0.16.25@sha256:74e5a7d55303ba525d939c6bf97ed4e010df7521f52d80afc22a815b66bd53f3" \
    "apache/james:memory-3.9.1@sha256:093b35e8e11ab63b379799d42cf4a1b095ba43f49da28de5c729c9e848b52e8e"
do
    docker pull --quiet "$image"
done
