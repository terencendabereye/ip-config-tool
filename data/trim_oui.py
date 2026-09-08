import csv

seen = set()
rows = []
with open("oui_raw.csv", newline="", encoding="utf-8-sig") as f:
    reader = csv.DictReader(f)
    for row in reader:
        prefix = row["Assignment"].strip().upper()
        if len(prefix) != 6 or not all(c in "0123456789ABCDEF" for c in prefix):
            continue
        name = row["Organization Name"].strip()
        if not name:
            continue
        # Trim overly long vendor names (a handful of legal-entity-style names
        # run 80+ chars); 60 chars is plenty to identify the vendor at a glance.
        if len(name) > 60:
            name = name[:57] + "..."
        # Escape any stray comma (rare) by dropping it rather than quoting,
        # to keep the embedded format a trivial two-field split on ','.
        name = name.replace(",", " ").replace("\n", " ").replace("\r", " ")
        if prefix in seen:
            continue
        seen.add(prefix)
        rows.append((prefix, name))

rows.sort(key=lambda r: r[0])

with open("oui.csv", "w", newline="", encoding="utf-8") as f:
    for prefix, name in rows:
        f.write(f"{prefix},{name}\n")

print(f"{len(rows)} entries written")
