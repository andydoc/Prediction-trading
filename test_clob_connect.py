import yaml, json
from py_clob_client.client import ClobClient

# Load secrets
with open('config/secrets.yaml') as f:
    secrets = yaml.safe_load(f)['polymarket']

print(f"Host: {secrets['host']}")
print(f"Funder: {secrets['funder_address'][:10]}...{secrets['funder_address'][-6:]}")
print(f"Sig type: {secrets['signature_type']}")
print()

# Initialize client
client = ClobClient(
    secrets['host'],
    key=secrets['private_key'],
    chain_id=secrets['chain_id'],
    signature_type=secrets['signature_type'],
    funder=secrets['funder_address']
)

# Derive API creds
print("Deriving API credentials...")
creds = client.create_or_derive_api_creds()
client.set_api_creds(creds)
print(f"API Key: {creds.api_key[:12]}...")
print()

# Test 1: Server time
print("=== Test 1: Server connectivity ===")
try:
    st = client.get_server_time()
    print(f"Server time: {st}")
except Exception as e:
    print(f"Error: {e}")

# Test 2: Price fetch from a real market
print("\n=== Test 2: Price fetch ===")
try:
    markets = json.loads(open('data/latest_markets.json').read())
    test_token = None
    test_name = None
    for m in markets[:100]:
        tokens = m.get('tokens', [])
        if tokens and len(tokens) > 0:
            tid = tokens[0].get('token_id')
            if tid and len(tid) > 10:
                test_token = tid
                test_name = m.get('question', '?')[:60]
                break
    if test_token:
        print(f"Market: {test_name}")
        print(f"Token: {test_token[:20]}...")
        mid = client.get_midpoint(test_token)
        print(f"Midpoint: {mid}")
    else:
        print("No test token found")
except Exception as e:
    print(f"Price error: {e}")

# Test 3: Open orders
print("\n=== Test 3: Open orders ===")
try:
    orders = client.get_orders()
    n = len(orders) if isinstance(orders, list) else 'unknown'
    print(f"Open orders: {n}")
except Exception as e:
    print(f"Orders error: {e}")

print("\n=== DONE ===")
