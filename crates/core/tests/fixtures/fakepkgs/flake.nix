{
  # A stand-in for nixpkgs, so the changelog lookup can be tested against the REAL `nix`
  # without fetching a real nixpkgs (~50 MB and a slow evaluation, for two metadata fields).
  #
  # What matters here is the shapes `meta.changelog` takes in the wild, plus the ones that
  # would take a whole batch down with them if the generated expression stopped guarding
  # against them: a package that throws on access, a changelog list containing a throw, and
  # a value `--json` cannot serialise.
  description = "Fake package set for changelog lookup tests";

  outputs =
    { self }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAll = f: builtins.listToAttrs (map (s: { name = s; value = f s; }) systems);
    in
    {
      legacyPackages = forAll (_: {
        plain.meta.changelog = "https://example.invalid/plain";

        # nixpkgs sometimes records several URLs; the first usable one wins.
        listed.meta.changelog = [
          "https://example.invalid/first"
          "https://example.invalid/second"
        ];

        # Present, but nothing to link to.
        nochangelog.meta.description = "no changelog here";
        emptychangelog.meta.changelog = "";
        nullchangelog.meta.changelog = null;

        # Accessing the attribute throws — an alias that errors out, an unsupported
        # package. `tryEval` has to absorb this.
        throwing = throw "this package refuses to evaluate";

        # The throw is INSIDE the list, so it only surfaces when elements are forced.
        # Forcing has to happen inside the `tryEval`, not later in `--json`.
        throwinglist.meta.changelog = [ (throw "inner boom") ];

        # `--json` can serialise neither of these; without the type guard, either one
        # fails the entire evaluation and every other package loses its link.
        functionchangelog.meta.changelog = x: x;
        mixedlist.meta.changelog = [ "https://example.invalid/good" (x: x) ];
      });
    };
}
