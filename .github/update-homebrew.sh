#!/usr/bin/env bash
# Updates the Homebrew formula in abhay/homebrew-tap via the GitHub API.
# Expects: TAG_NAME (e.g. v0.1.2), TAP_TOKEN, GITHUB_REPOSITORY, and release tarballs in CWD.
set -euo pipefail

VERSION="${TAG_NAME#v}"
URL="https://github.com/${GITHUB_REPOSITORY}/releases/download/${TAG_NAME}"

sha() { sha256sum "$1" | cut -d' ' -f1; }

cat > /tmp/formula.rb <<EOF
class ClaudeUsage < Formula
  desc "CLI for tracking Claude usage windows: status bar, token tracking, defer logic"
  homepage "https://github.com/${GITHUB_REPOSITORY}"
  version "${VERSION}"
  license "MIT"

  on_macos do
    on_arm do
      url "${URL}/claude-usage-${TAG_NAME}-aarch64-apple-darwin.tar.gz"
      sha256 "$(sha "claude-usage-${TAG_NAME}-aarch64-apple-darwin.tar.gz")"
    end

    on_intel do
      url "${URL}/claude-usage-${TAG_NAME}-x86_64-apple-darwin.tar.gz"
      sha256 "$(sha "claude-usage-${TAG_NAME}-x86_64-apple-darwin.tar.gz")"
    end
  end

  on_linux do
    on_arm do
      url "${URL}/claude-usage-${TAG_NAME}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "$(sha "claude-usage-${TAG_NAME}-aarch64-unknown-linux-musl.tar.gz")"
    end

    on_intel do
      url "${URL}/claude-usage-${TAG_NAME}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "$(sha "claude-usage-${TAG_NAME}-x86_64-unknown-linux-musl.tar.gz")"
    end
  end

  def install
    bin.install "claude-usage"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/claude-usage --version")
  end
end
EOF

# Push to homebrew-tap via GitHub API
FILE_SHA=$(curl -sf -H "Authorization: token ${TAP_TOKEN}" \
  "https://api.github.com/repos/abhay/homebrew-tap/contents/Formula/claude-usage.rb" \
  | jq -r '.sha')

jq -n --arg msg "Update claude-usage to ${TAG_NAME}" \
       --arg content "$(base64 -w0 /tmp/formula.rb)" \
       --arg sha "${FILE_SHA}" \
       '{message: $msg, content: $content, sha: $sha}' | \
curl -sf -X PUT \
  -H "Authorization: token ${TAP_TOKEN}" \
  -H "Content-Type: application/json" \
  "https://api.github.com/repos/abhay/homebrew-tap/contents/Formula/claude-usage.rb" \
  -d @-

echo "Homebrew formula updated to ${TAG_NAME}"
