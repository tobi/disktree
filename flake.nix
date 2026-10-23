{
  description = "disktree — GPUI treemap explorer for disk usage";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # nixpkgs 26.11 dropped x86_64-darwin; the 26.05 stable branch still
      # supports it until the end of 2026 if Intel Macs ever matter.
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
            }
          )
        );
    in
    {
      packages = forAllSystems (
        pkgs:
        rec {
          disktree = pkgs.rustPlatform.buildRustPackage {
            pname = "disktree";
            version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./.cargo
                ./crates
                ./xtask
                ./assets
                ./packaging
              ];
            };

            cargoLock.lockFile = ./Cargo.lock;

            # Only the application; building the whole workspace would also
            # ship the `xtask` development helper as a package binary.
            cargoBuildFlags = [ "-p" "disktree-app" ];

            nativeBuildInputs = [
              pkgs.pkg-config
              # gpui's macOS headers go through bindgen, which needs a
              # libclang; the hook points it at the Nix one instead of
              # whatever Command Line Tools happen to be on the host.
              pkgs.rustPlatform.bindgenHook
              pkgs.writableTmpDirAsHomeHook
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              # Both used by postFixup: the hook defines the
              # `addDriverRunpath` shell function, makeWrapper the
              # `wrapProgram` one.
              pkgs.addDriverRunpath
              pkgs.makeWrapper
            ];

            # GPUI's Linux backends link and run against these; macOS needs
            # no extras because the Metal/AppKit crates come through objc2.
            buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.fontconfig
              pkgs.freetype
              pkgs.libxkbcommon
              pkgs.wayland
              pkgs.vulkan-loader
              pkgs.libGL
              pkgs.libx11
              pkgs.libxcb
              pkgs.libxcursor
              pkgs.libxi
              pkgs.libxrandr
            ];

            # GPUI dlopens libvulkan, libEGL and libwayland-client at
            # runtime; dlopen ignores buildInputs, so point RUNPATH at them.
            #
            # Finding those loaders is only half of it: each one then has to
            # find a *driver*, which it discovers through a JSON manifest
            # naming the vendor library. On NixOS those manifests live under
            # `/run/opengl-driver`, the path `addDriverRunpath` appends; on
            # every other distribution that path does not exist, the system's
            # own manifests are invisible to a Nix-built loader, and
            # wgpu comes up with no backend at all — `create_surface` fails
            # with "Failed to create surface for any enabled backend: {}"
            # before a window is ever shown. Naming this closure's Mesa as an
            # *additional* source of drivers fixes those hosts and changes
            # nothing on NixOS: `VK_ADD_DRIVER_FILES` is searched after the
            # ICDs the loader discovers for itself, and the two GL variables
            # are suffixed rather than set. A machine driven by the
            # proprietary NVIDIA stack is the exception — Mesa cannot drive
            # that card, so those need nixGL or an equivalent.
            postFixup = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              patchelf --add-rpath ${
                pkgs.lib.makeLibraryPath [
                  pkgs.vulkan-loader
                  pkgs.libGL
                  pkgs.wayland
                ]
              } $out/bin/disktree
              addDriverRunpath $out/bin/disktree
              wrapProgram $out/bin/disktree \
                --suffix VK_ADD_DRIVER_FILES : ${pkgs.mesa}/share/vulkan/icd.d \
                --suffix __EGL_VENDOR_LIBRARY_DIRS : ${pkgs.mesa}/share/glvnd/egl_vendor.d \
                --suffix LIBGL_DRIVERS_PATH : ${pkgs.mesa}/lib/dri
            '';

            # Matches `make install` on Linux: icon and desktop entry.
            postInstall = pkgs.lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              install -Dm644 assets/disktree.svg \
                $out/share/icons/hicolor/scalable/apps/disktree.svg
              mkdir -p $out/share/applications
              substitute packaging/disktree.desktop.in \
                $out/share/applications/disktree.desktop \
                --subst-var-by BINDIR $out/bin --subst-var-by VERSION "$version"
            '';

            # `disktree-app` tests drive a real window harness; they cannot
            # run in the build sandbox. Two core tests read the host: one
            # assumes home sits on the root volume (false in the sandbox,
            # where TMPDIR lives on the Nix Store APFS volume), the other
            # execs /sbin/mount, which the sandbox refuses.
            doCheck = true;
            cargoTestFlags = [ "-p" "disktree-core" ];
            checkFlags = [
              "--skip=the_home_disk_on_macos_is_the_root_and_has_a_device"
              "--skip=this_macs_mount_table_is_read_without_proc"
            ];

            meta = {
              description = "GPUI treemap explorer for disk usage";
              homepage = "https://github.com/tobi/disktree";
              license = pkgs.lib.licenses.mit;
              mainProgram = "disktree";
              platforms = pkgs.lib.platforms.linux ++ pkgs.lib.platforms.darwin;
            };
          };
          default = disktree;
        }
      );
    };
}
