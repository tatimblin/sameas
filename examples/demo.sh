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
# NOTE: the place_id here is an illustrative placeholder, not a real Google id.
# Offline fixtures are matched by endpoint path, so the value is arbitrary; a
# live run (--fetch) would use a real place_id from Google.
echo "\$ sameas resolve --place-id 'EXAMPLE_blue_bottle_oakland'"
sameas resolve --place-id "EXAMPLE_blue_bottle_oakland"

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
RESTAURANT_ID="$(sameas --json resolve --place-id 'EXAMPLE_blue_bottle_oakland' | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
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
PLACE_ID="$(sameas --json resolve --place-id 'EXAMPLE_blue_bottle_oakland' | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "(.*)",?/\1/')"
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
echo "STEP 7  M2 — Hub bootstrapping: complete from external hubs, NO prior graph state"
echo "        (offline: hub responses served from examples/fixtures/hubs)"
DB2="${TMPDIR:-/tmp}/sameas-demo-hubs.db"
rm -f "$DB2"
HUBS="$ROOT/examples/fixtures/hubs"
sameas2() { "$BIN" --db "$DB2" "$@"; }

echo
echo "\$ sameas resolve --imdb tt0133093 --complete --hub-fixtures examples/fixtures/hubs"
sameas2 resolve --imdb tt0133093 --complete --hub-fixtures "$HUBS"
echo "  ^ IMDb id alone -> Wikidata QID + TMDb + website, from an EMPTY graph."

hr
echo "STEP 7b place_id -> website + phone via Google Place Details"
echo "\$ sameas resolve --place-id EXAMPLE_blue_bottle_oakland --complete --hub-fixtures examples/fixtures/hubs"
sameas2 resolve --place-id EXAMPLE_blue_bottle_oakland --complete --hub-fixtures "$HUBS"

hr
echo "STEP 7c name + full address -> Placekey anchor + place_id (reverse) then website + phone"
echo "        (Placekey needs a street; name+city alone resolves via place_id at low confidence)"
echo "\$ sameas resolve --name 'Blue Bottle Coffee' --address '300 Webster St' --city Oakland --region CA --country US --complete --hub-fixtures examples/fixtures/hubs"
sameas2 resolve --name "Blue Bottle Coffee" --address "300 Webster St" --city Oakland --region CA --country US --complete --hub-fixtures "$HUBS"

hr
echo "STEP 7d Same name query again — served from the LOCAL name index (no --complete,"
echo "        no hub fixtures) => zero external calls. Type-agnostic: qualifier is any facet."
echo "\$ sameas resolve --name 'Blue Bottle Coffee' --address '300 Webster St' --city Oakland --region CA --country US"
sameas2 resolve --name "Blue Bottle Coffee" --address "300 Webster St" --city Oakland --region CA --country US
echo "  ^ confidence_reason 'local_name_match' — resolved from the graph, nothing reached out."

hr
echo "Exit-criteria check: IMDb completes to a QID + TMDb from an empty graph:"
IMDB_OUT="$(sameas2 --json resolve --imdb tt0133093 --complete --hub-fixtures "$HUBS")"
if echo "$IMDB_OUT" | grep -q '"wikidata:Q83495"' && echo "$IMDB_OUT" | grep -q '"tmdb:603"'; then
  echo "  RESULT: OK ✓ (imdb -> wikidata:Q83495 + tmdb:603 + website)"
else
  echo "  RESULT: FAIL ✗"
  exit 1
fi

hr
echo "STEP 8  M3 — Disambiguation: same name/domain, different real-world things"
DB3="${TMPDIR:-/tmp}/sameas-demo-m3.db"
rm -f "$DB3"
MISS="$ROOT/examples/fixtures/hubs_miss"
sameas3() { "$BIN" --db "$DB3" "$@"; }

echo
echo "\$ sameas ingest examples/seed/kibatsu-sf.json ; ingest examples/seed/kibatsu-oakland.json"
sameas3 ingest "$SEED/kibatsu-sf.json"      >/dev/null
sameas3 ingest "$SEED/kibatsu-oakland.json" >/dev/null
echo "  (two Kibatsu locations that SHARE the domain kibatsu.com)"

SF="$(sameas3 --json resolve --place-id KIBATSU_SF  | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "?([^",]*)"?,?/\1/')"
OAK="$(sameas3 --json resolve --place-id KIBATSU_OAK | grep '"canonical_id"' | head -n1 | sed -E 's/.*: "?([^",]*)"?,?/\1/')"
echo "  place_id KIBATSU_SF  -> $SF"
echo "  place_id KIBATSU_OAK -> $OAK"
if [[ "$SF" != "$OAK" ]]; then
  echo "  RESULT: DISTINCT ✓ (a shared domain does NOT collapse two locations)"
else
  echo "  RESULT: COLLAPSED ✗"; exit 1
fi

hr
echo "STEP 8b The shared domain still resolves (to one location, single-valued)"
echo "\$ sameas resolve --domain kibatsu.com"
sameas3 resolve --domain kibatsu.com

hr
echo "STEP 8c Too little to be sure -> REFUSE (confidence is a control signal)"
echo "\$ sameas resolve --name 'Ghost Kitchen' --city Nowhere --complete --hub-fixtures examples/fixtures/hubs_miss"
sameas3 resolve --name "Ghost Kitchen" --city Nowhere --complete --hub-fixtures "$MISS"
echo "  ^ no resolvable identifier -> status 'unresolved' + reason: the caller should"
echo "    ask its end user for a stronger identifier, then re-resolve."

hr
echo "Demo complete. Throwaway DBs: $DB , $DB2 , $DB3"
