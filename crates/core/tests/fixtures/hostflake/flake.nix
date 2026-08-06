{
  # A tiny, network-free fixture flake for integration tests.
  #  * Two path inputs so `flake_input_names` has something to list.
  #  * Two pairs of "system" derivations, each pair differing only in one sub-derivation's
  #    version. `parentOld`/`parentNew` differ in `foo` (1.0 -> 2.0), which is what
  #    `nix store diff-closures` is pointed at. `structuredOld`/`structuredNew` differ in
  #    `bar`, which sets `__structuredAttrs` so that its pname/version live in nix's
  #    structured metadata rather than the environment — the shape the inventory diff has
  #    to read.
  #
  # Everything here is evaluated/instantiated only — never built — so no builder, network,
  # or nixpkgs is required. The system is hardcoded to x86_64-linux because a derivation
  # can be *instantiated* (drvPath) for any system regardless of the host architecture.
  inputs = {
    dep1.url = "path:./dep1";
    dep2.url = "path:./dep2";
  };

  outputs =
    { self, dep1, dep2 }:
    let
      system = "x86_64-linux";
      mkFoo =
        ver:
        derivation {
          name = "foo-${ver}";
          inherit system;
          builder = "/bin/sh";
          args = [ "-c" "true" ];
        };
      mkParent =
        fooVer:
        derivation {
          name = "parent-0";
          inherit system;
          builder = "/bin/sh";
          args = [ "-c" "true" ];
          # Referencing a derivation as an attr makes it a build input, so its .drv lands
          # in this derivation's closure — which is what diff-closures walks.
          foo = mkFoo fooVer;
        };

      # The same idea, but the sub-derivation sets `__structuredAttrs`, so nix keeps its
      # pname/version in structured metadata instead of the environment. That is the shape
      # that made an upgraded `bind` report as removed, and it is only worth testing against
      # the real nix, since the whole risk is what nix chooses to emit.
      mkBar =
        ver:
        derivation {
          name = "bar-${ver}";
          inherit system;
          builder = "/bin/sh";
          args = [ "-c" "true" ];
          __structuredAttrs = true;
          pname = "bar";
          version = ver;
        };
      mkStructuredParent =
        barVer:
        derivation {
          name = "structured-parent-0";
          inherit system;
          builder = "/bin/sh";
          args = [ "-c" "true" ];
          bar = mkBar barVer;
        };
    in
    {
      packages.${system} = {
        parentOld = mkParent "1.0";
        parentNew = mkParent "2.0";
        structuredOld = mkStructuredParent "1.0";
        structuredNew = mkStructuredParent "2.0";
      };
    };
}
