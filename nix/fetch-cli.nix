# Fetch an official pulumi/pulumi CLI release. The tarball bundles the CLI
# and every language host (pulumi-language-python, pulumi-resource-pulumi-python,
# …) in one directory; Pulumi discovers binaries living next to its own
# executable without warnings, exactly like an official install — so using
# this keeps SDK, CLI, and language host versions in lockstep with uv.lock
# and needs no language entries in the plugin store.
{ lib, systemToTarget }:
{ pkgs
, version
, hash
, target ? systemToTarget.${pkgs.stdenv.hostPlatform.system}
, baseURL ? "https://github.com/pulumi/pulumi/releases/download/v${version}"
}:
pkgs.stdenv.mkDerivation {
  pname = "pulumi-cli";
  inherit version;

  src = pkgs.fetchurl {
    # CLI release assets use x64 where provider releases use amd64.
    url = "${baseURL}/pulumi-v${version}-${lib.replaceStrings [ "amd64" ] [ "x64" ] target}.tar.gz";
    inherit hash;
  };

  sourceRoot = "pulumi";
  dontConfigure = true;
  dontBuild = true;

  nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
    pkgs.autoPatchelfHook
  ];

  # The Go binaries are static, but pulumi-watch (Rust) links libgcc_s.
  buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
    pkgs.stdenv.cc.cc.lib
  ];

  # pulumi-language-python-exec et al. are invoked through the resolved
  # toolchain interpreter, never via their shebangs — leave them pristine.
  dontPatchShebangs = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp -r ./. $out/bin/
    chmod +x $out/bin/*
    runHook postInstall
  '';

  meta = {
    description = "Pulumi CLI and language hosts (official release binaries)";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = builtins.attrNames systemToTarget;
    mainProgram = "pulumi";
  };
}
