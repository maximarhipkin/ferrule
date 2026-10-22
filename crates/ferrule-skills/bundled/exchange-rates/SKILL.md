---
name: exchange-rates
description: Currency exchange rates and conversions, today's or on a past date, from the free Frankfurter API with no key (central bank reference rates such as the ECB's). Use to convert an amount between currencies or for a currency's rate or its history.
license: MIT OR Apache-2.0
compatibility: Ships with ferrule. Needs curl and network access from the sandboxed shell.
---

# Exchange rates from Frankfurter

Frankfurter (`api.frankfurter.dev`) publishes central banks' reference
rates, updated once a working day, with no key and no quota (it limits
abuse). These are reference rates, not what a bank or card charges. On
Windows (PowerShell) write `curl.exe`, not `curl`.

## One pair

```sh
curl -s https://api.frankfurter.dev/v2/rate/usd/ils
```

The answer is `{"date":"2026-10-21","base":"USD","quote":"ILS","rate":3.0966}`.
There is no conversion call: multiply the amount by `rate` yourself.

## Several at once

```sh
curl -s "https://api.frankfurter.dev/v2/rates?base=usd&quotes=eur,gbp,ils"
```

The answer is a list of `{"date","base","quote","rate"}`.

- A past day: add `&date=2025-01-31`. Weekends and holidays have no rates
  of their own; the answer's `date` says which day you got.
- A period: `&from=2026-01-01&to=2026-03-31` (add `&group=week` or
  `&group=month` for fewer points).
- Only the European Central Bank's rates (the default blends several
  central banks):
  `https://api.frankfurter.dev/v2/providers/ecb/rates?base=eur&quotes=usd`.
- Which currencies exist: `https://api.frankfurter.dev/v2/currencies`.

Codes are ISO 4217 (`usd`, `eur`, `ils`, `jpy`); lowercase works.

## Answering

- Give the converted amount, the rate, and the date of the rate.
- Say it is a reference rate: a bank, a card or an exchange office adds its
  own margin.
- Frankfurter doesn't cover a currency: say so; don't guess a rate.
