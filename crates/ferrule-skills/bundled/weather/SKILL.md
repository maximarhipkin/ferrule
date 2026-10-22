---
name: weather
description: Current weather and forecasts up to 16 days ahead for any place, from Open-Meteo, with no key. Use for questions about the weather, temperature, rain, snow, wind or a forecast.
license: MIT OR Apache-2.0
compatibility: Ships with ferrule. Needs curl and network access from the sandboxed shell.
---

# Weather from Open-Meteo

Open-Meteo's free API takes no key. Its terms: non-commercial use, fewer
than 10,000 calls a day, and credit to Open-Meteo (CC BY 4.0) wherever its
data is shown. One question takes two calls at most: find the place, then
get its forecast. Run them with the shell; on Windows (PowerShell) write
`curl.exe`, not `curl`.

## 1. Find the place

```sh
curl -s "https://geocoding-api.open-meteo.com/v1/search?name=Haifa&count=3&language=en&format=json"
```

- `name` is the place, URL-encoded (`New%20York`). Add `&countryCode=IL`
  (two letters, ISO 3166-1) when the country is known.
- Each result has `name`, `country`, `admin1` (the region; may be
  missing), `latitude`, `longitude` and `timezone`. When several fit and
  the question doesn't say which, ask.
- No result: try the place's English name, or the nearest city.
- Skip this step when you already have coordinates.

## 2. Get the forecast

```sh
curl -s "https://api.open-meteo.com/v1/forecast?latitude=32.79&longitude=34.99&current=temperature_2m,apparent_temperature,relative_humidity_2m,precipitation,weather_code,wind_speed_10m&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,precipitation_probability_max,wind_speed_10m_max&timezone=auto&forecast_days=3"
```

- `forecast_days` is 1 to 16 (7 when left out). Ask only for the days the
  question is about.
- `timezone=auto` gives the place's local dates and times; `daily` needs a
  timezone.
- Units: add `temperature_unit=fahrenheit`, `wind_speed_unit=mph` (or
  `ms`, `kn`) and `precipitation_unit=inch` for someone who uses them.
  The defaults are °C, km/h and mm.
- Hour by hour ("will it rain this afternoon?"):
  `hourly=temperature_2m,precipitation_probability,weather_code&forecast_hours=12`.

`weather_code` is a WMO code; `references/weather-codes.md` says what each
one means.

## Answering

- Name the place you looked up (name, region, country), so a wrong match
  shows.
- Give the numbers with their units, rounded sensibly, and the local time
  or day they are for.
- End with "Weather data by Open-Meteo.com".
- A failed call: say what failed (no such place, the service didn't
  answer, ferrule's network policy refused the host) rather than guessing
  the weather.
