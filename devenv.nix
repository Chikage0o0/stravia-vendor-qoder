{ pkgs, ... }:

{
  packages = with pkgs; [
    go-task
    python3
    git
  ];

  # rust-toolchain.toml 是工具链版本、组件与 wasm32-wasip2 目标的唯一来源，与 CI 一致。
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };
}
