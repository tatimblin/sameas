#!/usr/bin/env bash
#
# sameas M1 demo — offline, deterministic.
#
# Story: one identifier in -> canonical id + the OTHER identifiers out
# (completion); the same entity reached from phone, place_id, and domain
# (stable identity + matched_via); phone corroborates but never merges.
#
# Runs against a throwaway DB that is deleted at start.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB="${TMPDIR:-/tmp}/sameas-demo.db"
rm -f "$DB"

echo "==> Building sameas (debug) ..."
cargo build --quiet --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/debug/sameas"
SEED="$ROOT/examples/seed"
FIXTURES="$ROOT/examples/fixtures"

sameas() { "$BIN" --db "$DB" "$@"; }

hr() { printf '\n%s\n' "----------------------------------------------------------------------"; }

hr
echo "STEP 1  Seed one restaurant entity (domain + place_id + phone + wikidata)"
echo "\$ sameas ingest examples/seed/blue-bottle.json"
sameas ingest "$SEED/blue-bottle.json"

hr
echo "STEP 2  Partial info in -> canonical id + COMPLETION out (phone we never typed)"
echo "\$ sameas resolve --phone '+1-510-653-3394'"
sameas resolve --phone "+1-510-653-3394"

hr
echo "STEP 3  A DIFFERENT partial input resolves to the SAME entity (stable identity)"
echo "\$ sameas resolve --place-id 'ChIJN1t_tDeuEmsRUsoyG83frY4'"
sameas resolve --place-id "ChIJN1t_tDeuEmsRUsoyG83frY4"

hr
echo "STEP 4  Domain resolver harvests sameAs from a page fixture and links it in"
echo "\$ sameas resolve --domain bluebottlecoffee.com --fixture examples/fixtures/blue-bottle.html"
sameas resolve --domain bluebottlecoffee.com --fixture "$FIXTURES/blue-bottle.html"

hr
echo "STEP 4b Generic --id flag resolves ANY registered kind (Yelp, added via the registry)"
echo "\$ sameas resolve --id yelp:blue-bottle-coffee-san-francisco"
sameas resolve --id yelp:blue-bottle-coffee-san-francisco

hr
echo "STEP 5  Show the completed cluster"
RESTAURANT_ID="$(sameas --json resolve --place-id 'ChIJN1t_tDeuEmsRUsoyG83frY4' | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
echo "\$ sameas entity $RESTAURANT_ID"
sameas entity "$RESTAURANT_ID"

hr
echo "STEP 6  Movie path, same mechanic (ingest + resolve by imdb)"
echo "\$ sameas ingest examples/seed/the-matrix.json"
sameas ingest "$SEED/the-matrix.json"
echo "\$ sameas resolve --imdb tt0133093"
sameas resolve --imdb tt0133093

hr
echo "Identity stability check across steps 2-4b (phone / place_id / domain / yelp):"
PHONE_ID="$(sameas --json resolve --phone '+1-510-653-3394'          | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
PLACE_ID="$(sameas --json resolve --place-id 'ChIJN1t_tDeuEmsRUsoyG83frY4' | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
DOMAIN_ID="$(sameas --json resolve --domain bluebottlecoffee.com --fixture "$FIXTURES/blue-bottle.html" | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
YELP_ID="$(sameas --json resolve --id yelp:blue-bottle-coffee-san-francisco | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
echo "  phone    -> $PHONE_ID"
echo "  place_id -> $PLACE_ID"
echo "  domain   -> $DOMAIN_ID"
echo "  yelp     -> $YELP_ID"
if [[ "$PHONE_ID" == "$PLACE_ID" && "$PLACE_ID" == "$DOMAIN_ID" && "$DOMAIN_ID" == "$YELP_ID" ]]; then
  echo "  RESULT: STABLE ✓ (all four resolve to the same canonical id)"
else
  echo "  RESULT: MISMATCH ✗"
  exit 1
fi

hr
echo "Demo complete. Throwaway DB: $DB"
