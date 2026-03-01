from dataclasses import dataclass
import json

@dataclass  
class MarketData:
    market_id: str
    market_name: str
    
def test():
    print('File created successfully')
    
if __name__ == '__main__':
    test()
