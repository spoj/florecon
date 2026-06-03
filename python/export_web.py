import json

OUT = "web/data.json"

def main():
    out = {
        "pair": "MOCK",
        "plan": {
            "primary": "amount",
            "root": {"op": "exact"}
        },
        "fields": [],
        "display": [],
        "netKey": "amount",
        "arrowBytes": ""
    }
    with open(OUT, "w") as f:
        json.dump(out, f)

if __name__ == "__main__":
    main()
