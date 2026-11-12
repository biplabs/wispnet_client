cask "wispnet" do
  version "60.0fbc42a"
  sha256 "4c8d9787a9fd62b12084f839a5c0c0829bb95685599aa45debe755b30869c596"

  url "https://github.com/biplabs/wispnet_client/releases/download/development-60-0fbc42a/wispnet-macos-aarch64.dmg"
  name "Wispnet"
  desc "Share and access services across your devices"
  homepage "https://github.com/biplabs/wispnet_client"

  depends_on arch: :arm64
  depends_on macos: :ventura

  app "Wispnet.app"
  binary "#{appdir}/Wispnet.app/Contents/MacOS/wispnet-node", target: "wispnet"

  uninstall launchctl: ["app.wispnet.desktop", "app.wispnet.node"],
            quit:      "app.wispnet.desktop",
            delete:    [
              "~/Library/LaunchAgents/app.wispnet.desktop.plist",
              "~/Library/LaunchAgents/app.wispnet.node.plist",
              "~/.local/libexec/wispnet-node",
              "~/.local/bin/wispnet",
              "~/.local/bin/wispnet-node",
            ]

  caveats <<~EOS
    Open Wispnet from Applications once to set up its background node.
    Existing identity, permissions and settings are preserved on upgrade.
    This development build is ad hoc signed and is not notarized.
  EOS
end
