{
  # A tiny, network-free fixture flake for integration tests.
  #  * Two path inputs so `flake_input_names` has something to list.
  #  * Two "system" derivations whose closures differ only in a `foo` sub-derivation's
  #    version, so `nix store diff-closures` over their .drv paths yields `foo: 1.0 -> 2.0`.
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
    in
    {
      packages.${system} = {
        parentOld = mkParent "1.0";
        parentNew = mkParent "2.0";
      };
    };
}
