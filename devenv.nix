{ pkgs, config, ... }:

{
  packages = with pkgs; [
    just

    lld
    cargo-audit
    cargo-deny
    cargo-release
    cargo-watch
  ];

  languages = {
    rust = {
      enable = true;
      channel = "stable";
      targets = [ "wasm32-unknown-unknown" ];
    };
  };

  # Builds the smoke Worker (examples/smoke), at the version of `worker` in Cargo.lock.
  worker-build = {
    enable = true;

    # TODO: drop once nixpkgs has worker-build 0.9.0.
    package = pkgs.worker-build.overrideAttrs (
      finalAttrs: prev: {
        version = "0.9.0";

        src = pkgs.fetchFromGitHub {
          owner = "cloudflare";
          repo = "workers-rs";
          tag = "v${finalAttrs.version}";
          hash = "sha256-zmeQp7db2rwfECp9p5NthKvzsZFnQPFO+eueIhFsIwE=";
          fetchSubmodules = true;
        };

        cargoHash = "sha256-EbRwSrxRys2EcDfB88apmH5Rm0NUWLKtmGPS77fAhpI=";

        # zstd-sys links the system zstd through pkg-config.
        buildInputs = prev.buildInputs ++ [ pkgs.zstd ];
      }
    );
  };

  # `devenv up` serves the smoke Worker under `wrangler dev` (process `wrangler-smoke`).
  wrangler = {
    enable = true;

    dev.smoke.directory = "examples/smoke";
  };

  # `devenv test` starts the smoke Worker, then runs the smoke test against it.
  enterTest = ''
    just smoke "http://127.0.0.1:${toString config.processes.wrangler-smoke.ports.http.value}"
  '';
}
