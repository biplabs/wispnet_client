cask "wispnet" do
  arch arm: "aarch64", intel: "x86_64"

  version "47.937f088"
  sha256 arm:   "8bca3e00a8927f6c105c7b37839c17436a7dfa6784ac343585de7d591a2e0208",
         intel: "6dcd721910ce0ad533adbd930497b26b99efc0c17c0f5de939591c2748e731b0"

  url "https://github.com/biplabs/wispnet_client/releases/download/development-47-937f088/wispnet-macos-#{arch}.dmg"
  name "Wispnet"
  desc "Share and access services across your devices"
  homepage "https://github.com/biplabs/wispnet_client"

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
