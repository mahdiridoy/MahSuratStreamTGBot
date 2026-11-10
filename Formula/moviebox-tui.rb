class MovieboxTui < Formula
  VERSION = "0.1.29"
  MACOS_SHA256 = "6da8c929c35e6341f4abb04e8faa2d8ed4bce94e7d6bcc49d055ac71b247ef61"
  LINUX_X64_SHA256 = "e930016ca215f202ea9494c8063f1fcc52e92a0ea99e1e175d0562eea28f13a5"
  LINUX_ARM64_SHA256 = "76c04a16a8862d5c0379b6a5bd6fb3b344b6bf3fb01586c57e5f42cc4f8cac8d"

  desc "Stream movies, shows, anime, and live TV from your terminal"
  homepage "https://github.com/mesamirh/MovieBox-Tui"
  version VERSION
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    url "https://github.com/mesamirh/MovieBox-Tui/releases/download/v#{VERSION}/MovieBox_macOS_Universal.tar.gz"
    sha256 MACOS_SHA256
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/mesamirh/MovieBox-Tui/releases/download/v#{VERSION}/MovieBox_Linux_arm64.tar.gz"
      sha256 LINUX_ARM64_SHA256
    else
      url "https://github.com/mesamirh/MovieBox-Tui/releases/download/v#{VERSION}/MovieBox_Linux_x64.tar.gz"
      sha256 LINUX_X64_SHA256
    end
  end

  def install
    bin.install "moviebox-tui"
  end

  test do
    system "#{bin}/moviebox-tui", "--version"
  end
end
