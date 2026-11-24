#!/usr/bin/env bash
# Ubuntu's packaged Chromium uses Snap. Run the matching Debian browser/driver
# from the same signed repository on both hosted Linux architectures.
set -euo pipefail
wrapper="$RUNNER_TEMP/ox-browser-bin"
mkdir -p "$wrapper"
image=ox-headless-browser:ci
docker build -t "$image" - <<'DOCKER'
FROM debian:bookworm-slim@sha256:e67eeead736ff7bbe258ef088bff683635c561b1fcfefad0806c1df4ab49ce0e
RUN apt-get update -o Acquire::Retries=3 && \
    apt-get install -y --no-install-recommends -o Acquire::Retries=3 chromium chromium-driver && \
    rm -rf /var/lib/apt/lists/*
DOCKER
browser=$(docker run --rm "$image" chromium --version | awk '{print $2}')
driver=$(docker run --rm "$image" chromedriver --version | awk '{print $2}')
test "$browser" = "$driver"
echo "Matched Chromium and ChromeDriver $browser on $(uname -m)."
cat > "$wrapper/chromedriver" <<'SH'
#!/bin/sh
# Host networking lets Chromium reach the fixture's loopback TLS forwarder.
# A camera test may expose one generated Y4M file at the absolute path passed to Chromium.
if [ -n "${OX_TEST_CAMERA_FILE:-}" ]; then
    set -- --mount "type=bind,src=$OX_TEST_CAMERA_FILE,dst=$OX_TEST_CAMERA_FILE,readonly" ox-headless-browser:ci /usr/bin/chromedriver "$@"
else
    set -- ox-headless-browser:ci /usr/bin/chromedriver "$@"
fi
exec docker run --rm -i --init --network=host --cap-drop=ALL --security-opt=no-new-privileges \
    --read-only --tmpfs /tmp:rw,nosuid,nodev,mode=1777 \
    --user "$(id -u):$(id -g)" -e HOME=/tmp -e XDG_CONFIG_HOME=/tmp/config -e XDG_CACHE_HOME=/tmp/cache \
    "$@"
SH
chmod +x "$wrapper/chromedriver"
echo "$wrapper" >> "$GITHUB_PATH"
echo 'OX_TEST_CHROMIUM=/usr/bin/chromium' >> "$GITHUB_ENV"
echo 'OX_TEST_CHROMIUM_PROFILE=/tmp/ox-phone-profile' >> "$GITHUB_ENV"
