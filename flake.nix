{
  description = "ipfs_module: a bundled Kubo IPFS node for Logos Basecamp";

  inputs = {
    # Same builder rev as fileverse_core and eth_rpc_module 0.1.0.
    logos-module-builder.url = "github:logos-co/logos-module-builder/e1a25706edb50aa7f34d02e08262ae7ffb3cb8db";
  };

  outputs = inputs@{ logos-module-builder, ... }:
    logos-module-builder.lib.mkLogosModule {
      src = ./.;
      configFile = ./metadata.json;
      flakeInputs = inputs;
    };
}
