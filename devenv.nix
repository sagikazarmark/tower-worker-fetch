{ pkgs, config, ... }:

{
  dotenv.enable = true;

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
