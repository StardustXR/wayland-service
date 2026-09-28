{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      supportedSystems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      nixpkgsFor = forAllSystems (system: import nixpkgs { inherit system; });
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
          craneLib = crane.mkLib pkgs;
        in
        {
          default = craneLib.buildPackage {
            src = ./.;
            cargoLock = ./Cargo.lock;
            # also build display-socket-finder
            cargoExtraArgs = "--locked --workspace";

            buildInputs = [ pkgs.libxcb ];

            # vulkano dlopens libvulkan (guarded since crane's deps-only build runs this too)
            postFixup = ''
              if [ -e $out/bin/stardust-xr-wayland-service ]; then
                patchelf $out/bin/stardust-xr-wayland-service --add-rpath ${pkgs.vulkan-loader}/lib
              fi
            '';

            STARDUST_RES_PREFIXES = pkgs.stdenvNoCC.mkDerivation {
              name = "data";
              src = ./.;

              buildPhase = "cp -r $src/data $out";
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgsFor.${system};
        in
        {
          default = pkgs.mkShell {
            nativeBuildInputs = with pkgs; [
              cargo
              rustc
            ];
            buildInputs = [ pkgs.libxcb ];
          };
        }
      );
    };
}
