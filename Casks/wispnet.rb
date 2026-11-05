cask "wispnet" do
  version "49.20f5354"
  sha256 "1e11d0d7f69bbac28b13b4864035d369980e977294973eb87e70aa77c0552904"

  url "https://github.com/biplabs/wispnet_client/releases/download/development-49-20f5354/wispnet-macos-aarch64.dmg"
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
