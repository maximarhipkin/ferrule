---
name: maps
description: Finds places and addresses, turns coordinates into an address, and gives driving, cycling or walking distance and travel time between places, from OpenStreetMap services with no key (Nominatim and the FOSSGIS OSRM server). Use for where something is, an address, how far, or how long it takes to get there.
license: MIT OR Apache-2.0
compatibility: Ships with ferrule. Needs curl and network access from the sandboxed shell.
---

# Maps from OpenStreetMap, with no key

Two free community services. Both are run by volunteers and both have
rules this skill keeps:

- **At most one request a second** to each, and only what the question
  needs. Never search letter by letter as someone types, and never look up
  a long list of addresses: both are against Nominatim's policy.
- **Name the program in every request** (`-A` below); a library's default
  User-Agent is refused.
- **Credit OpenStreetMap**: end an answer that uses their data with
  "© OpenStreetMap contributors".
- Non-commercial, reasonable use, and no guarantee they answer.

On Windows (PowerShell) write `curl.exe`, not `curl`.

## Find a place (Nominatim)

```sh
curl -s -A "ferrule-maps/1.0 (+https://github.com/maximarhipkin/ferrule)" "https://nominatim.openstreetmap.org/search?q=Technion%20Haifa&format=jsonv2&limit=3&addressdetails=1&accept-language=en"
```

- `q` is the place or address, URL-encoded. `countrycodes=il,us` narrows
  it; `limit` is at most 40.
- Each result has `lat`, `lon` (strings), `display_name`, `type` and, with
  `addressdetails=1`, an `address` broken into parts.
- Several plausible matches and the question doesn't say which: ask.

## Coordinates to an address (Nominatim)

```sh
curl -s -A "ferrule-maps/1.0 (+https://github.com/maximarhipkin/ferrule)" "https://nominatim.openstreetmap.org/reverse?lat=32.7767&lon=35.0231&format=jsonv2&zoom=18&accept-language=en"
```

`zoom` sets the detail: 18 a building, 10 a city, 3 a country.

## Distance and travel time (OSRM)

The FOSSGIS server at `routing.openstreetmap.de` has one path per way of
travel: `routed-car`, `routed-bike` and `routed-foot`.

```sh
curl -s -A "ferrule-maps/1.0 (+https://github.com/maximarhipkin/ferrule)" "https://routing.openstreetmap.de/routed-car/route/v1/driving/34.99,32.79;35.21,31.77?overview=false&steps=false"
```

- Coordinates are **longitude first**: `lon,lat;lon,lat`. Find them with
  Nominatim first, one request a second.
- Keep `/route/v1/driving/` in the path for all three; the
  `routed-…` part picks the way of travel.
- The answer's `routes[0].distance` is in metres and `routes[0].duration`
  in seconds. `steps=true` adds turn-by-turn steps; ask for them only when
  directions are wanted, since they are long.
- `code` other than `"Ok"` (like `NoRoute`) means no route: say so.
- Durations are typical travel times with no live traffic.

## Answering

- Say which place each name matched (its `display_name`), so a wrong
  match shows.
- Distances in km or miles as the owner uses them, times rounded to
  minutes, and the way of travel.
- A map link the owner can open:
  `https://www.openstreetmap.org/?mlat=<lat>&mlon=<lon>#map=17/<lat>/<lon>`,
  or for a route
  `https://www.openstreetmap.org/directions?engine=fossgis_osrm_car&route=<lat1>,<lon1>;<lat2>,<lon2>`
  (`fossgis_osrm_bike`, `fossgis_osrm_foot`).
- End with "© OpenStreetMap contributors".
