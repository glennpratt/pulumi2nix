# Synthesize Pulumi's native plugin cache layout as a linkFarm:
#
#   plugins/resource-<name>-v<version>/pulumi-resource-<name>
#   plugins/language-<name>-v<version>/pulumi-language-<name>
#
# Linking whole plugin *directories* (not individual binaries) mirrors what
# `pulumi plugin install` produces, so the engine treats them as natively
# installed — no "from $PATH" warnings, no network acquisition.
{ lib }:
{ pkgs
, resourcePlugins ? [ ] # [{ name, version, drv }] — drv IS the plugin dir
, languageHosts ? [ ]   # [{ name, version, drv }] — binary at drv/bin/pulumi-language-<name>
}:
pkgs.linkFarm "pulumi-plugin-store" (
  map
    (p: {
      name = "plugins/resource-${p.name}-v${p.version}";
      path = p.drv;
    })
    resourcePlugins
  ++ map
    (l: {
      name = "plugins/language-${l.name}-v${l.version}";
      path = "${l.drv}/bin";
    })
    languageHosts
)
