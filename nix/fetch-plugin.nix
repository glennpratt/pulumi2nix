# Fetch a Pulumi resource plugin binary release and unpack it into a
# directory matching what `pulumi plugin install` would have produced under
# ~/.pulumi/plugins/resource-<name>-v<version>/.
{ lib, systemToTarget }:
{ pkgs
, name
, version
, hash
, target ? systemToTarget.${pkgs.stdenv.hostPlatform.system}
, baseURL ? "https://github.com/pulumi/pulumi-${name}/releases/download/v${version}"
}:
pkgs.stdenv.mkDerivation {
  pname = "pulumi-resource-${name}";
  inherit version;

  src = pkgs.fetchurl {
    url = "${baseURL}/pulumi-resource-${name}-v${version}-${target}.tar.gz";
    inherit hash;
  };

  # Plugin tarballs have no top-level directory; unpack into a clean subdir
  # so stdenv build artifacts (env-vars etc.) never leak into $out.
  unpackPhase = ''
    runHook preUnpack
    mkdir source
    tar -xzf "$src" -C source
    runHook postUnpack
  '';
  sourceRoot = "source";
  dontConfigure = true;
  dontBuild = true;

  # Pre-built binaries assume an FHS dynamic linker; fix them up on Linux.
  # Fully static Go binaries are left untouched by autoPatchelf.
  nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
    pkgs.autoPatchelfHook
  ];

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp -r ./. $out/
    chmod +x $out/pulumi-resource-${name}
    runHook postInstall
  '';

  meta = {
    description = "Pulumi resource plugin for ${name} (pre-built release binary)";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = builtins.attrNames systemToTarget;
  };
}
