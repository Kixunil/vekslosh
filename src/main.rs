#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate log;

use std::sync::Arc;
use std::fmt;
use std::collections::HashMap;
use std::path::Path;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use earn_percentage::SatEarnPercentage;
use futures::lock::Mutex;

const STATIC_DIR: &str = "static";

type PinBoxFuture<T, E> = std::pin::Pin<Box<dyn std::future::Future<Output=Result<T, E>> + Send + Sync>>;

mod earn_percentage {
    pub struct SatEarnPercentage(f64);

    impl SatEarnPercentage {
        pub fn new(value: f64) -> Self {
            if value.is_nan() || value > 100.0 || value < 0.0 {
                panic!("invalid value {}", value);
            }

            SatEarnPercentage(value / 100.0)
        }

        pub fn gov_shitcoin_earn_percentage(self) -> f64 {
            1.0 - self.0
        }

        pub fn into_inner(self) -> f64 {
            self.0
        }
    }
}

/*
mod fee {
    pub struct Fee(f64);

    impl Fee {
        pub fn new(value: f64) -> Self {
            if value.is_nan() || value > 1.0 || value < 0.0 {
                panic!("invalid value {}", value);
            }

            Ratio(value)
        }

        pub fn invert(self) -> Self {
            Fee(1 - self.0)
        }

        pub fn into_inner(self) -> f64 {
            self.0
        }
    }

    impl std::ops::Mul for Fee {
        type Output = Self;

        fn mul(self, rhs: Fee) -> Self::Output {
            Ratio(self.0 * rhs.0)
        }
    }
}
*/

#[derive(Eq, PartialEq, Copy, Clone, Debug)]
struct GovShitcoinId(usize);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Currency {
    Sats,
    GovShitcoin(GovShitcoinId),
}

type Price = f64;

struct ConversionTable {
    rates: Vec<Price>,
    btc: Price,
    gov_shitcoins: HashMap<String, GovShitcoinId>,
}

impl ConversionTable {
    pub fn new(btc_price: Price, native_gov_shitcoin_name: String) -> Self {
        let mut gov_shitcoins = HashMap::new();
        gov_shitcoins.insert(native_gov_shitcoin_name, GovShitcoinId(0));

        ConversionTable {
            rates: Vec::new(),
            btc: btc_price / 100_000_000.0,
            gov_shitcoins,
        }
    }

    pub fn native_currency(&self) -> Currency {
        Currency::GovShitcoin(GovShitcoinId(0))
    }

    fn shitcoin_rate(&self, id: GovShitcoinId) -> Price {
        if id.0 == 0 {
            1.0
        } else {
            self.rates[id.0 - 1]
        }
    }

    pub fn register_shitcoin(&mut self, rate: Price, name: String) -> GovShitcoinId {
        self.rates.push(rate);
        let id = self.rates.len();
        self.gov_shitcoins.insert(name, GovShitcoinId(id));
        GovShitcoinId(id)
    }

    pub fn update_price(&mut self, id: GovShitcoinId, price: Price) {
        self.rates[id.0] = price;
    }

    pub fn update_btc_price(&mut self, price: Price) {
        self.btc = price;
    }

    pub fn to_native(&self, amount: Amount) -> Amount {
        match amount.currency {
            Currency::Sats => Amount {
                value: (amount.value / 100_000_000.0 * self.btc).round(),
                currency: self.native_currency(),
            },
            Currency::GovShitcoin(shitcoin_id) => Amount {
                value: (amount.value * self.shitcoin_rate(shitcoin_id)).round(),
                currency: self.native_currency(),
            },
        }
    }

    pub fn from_native(&self, amount: f64, to: Currency) -> Amount {
        match to {
            Currency::Sats => panic!("This shouldn't be used"),
            Currency::GovShitcoin(shitcoin_id) => Amount {
                value: (amount / self.shitcoin_rate(shitcoin_id)).round(),
                currency: to,
            },
        }
    }

    pub fn decode_currency(&self, name: &str) -> Option<Currency> {
        if name == "sats" {
            Some(Currency::Sats)
        } else {
            self.gov_shitcoins.get(name).map(|id| Currency::GovShitcoin(*id))
        }
    }
}

#[derive(Copy, Clone)]
struct Amount {
    value: f64,
    currency: Currency,
}

impl std::ops::Neg for Amount {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Amount {
            value: -self.value,
            currency: self.currency,
        }
    }
}


impl std::ops::Mul<f64> for Amount {
    type Output = Self;

    fn mul(self, rhs: f64) -> Self::Output {
        Amount {
            value: (self.value * rhs).round(),
            currency: self.currency,
        }
    }
}

fn compute_hedge_amount(amount: Amount, fee: f64, sat_fee_percentage: SatEarnPercentage, conversions: &ConversionTable, direction: Direction) -> Amount {
    let to_hedge = match (amount.currency, direction) {
        (Currency::Sats, Direction::Buy) => amount * (1.0 - fee * sat_fee_percentage.gov_shitcoin_earn_percentage()),
        (Currency::GovShitcoin(_), Direction::Sell) => amount * (1.0 - fee * sat_fee_percentage.gov_shitcoin_earn_percentage()),
        (Currency::Sats, Direction::Sell) => -amount * (1.0 + fee * sat_fee_percentage.gov_shitcoin_earn_percentage()),
        (Currency::GovShitcoin(_), Direction::Buy) => -amount * (1.0 + fee * sat_fee_percentage.gov_shitcoin_earn_percentage()),
    };
    conversions.to_native(to_hedge)
}

enum RequestError {
    Internal,
    Client { code: StatusCode, message: &'static str, error: Option<Box<dyn std::error::Error>>, },
    Hyper(hyper::Error),
}

impl From<hyper::Error> for RequestError {
    fn from(value: hyper::Error) -> Self {
        RequestError::Hyper(value)
    }
}


impl RequestError {
    fn not_found() -> Self {
        RequestError::Client { code: StatusCode::NOT_FOUND, message: "path not found", error: None, }
    }

    fn simple(code: u16, message: &'static str) -> Self {
        RequestError::Client { code: StatusCode::from_u16(code).unwrap(), message, error: None, }
    }

    fn into_response(self) -> Result<Response<Body>, hyper::Error> {
        use std::fmt::Write;

        let mut response = Response::default();

        match self {
            RequestError::Internal => {
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            },
            RequestError::Client { code, message, error, } => {
                let mut text = String::from(message);
                if let Some(error) = error {
                    write!(&mut text, ": ").unwrap();
                    write_error(&mut text, &*error).unwrap();
                }

                let body = Body::from(text);
                *response.status_mut() = code;
                *response.body_mut() = body;
            },
            RequestError::Hyper(error) => return Err(error),
        }

        Ok(response)
    }
}

fn write_error<W: fmt::Write, E: std::error::Error + ?Sized>(mut writer: W, error: &E) -> fmt::Result {
    write!(writer, "{}", error)?;
    let mut source = error.source();
    while let Some(error) = source {
        write!(writer, ": {}", error)?;
        source = error.source();
    }
    Ok(())
}

struct DebugError<E>(E);

impl<E: std::error::Error> fmt::Debug for DebugError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write_error(f, &self.0)
    }
}

struct Hedged {
    hedge_price: f64,
    liquidation_price: f64,
}

#[derive(Serialize)]
struct HedgeResponse {
    hedged_amount: f64,
    hedge_price: f64,
    liquidation_price: f64,
    client_to_vekslak: f64,
    vekslak_to_client: f64,
}

trait ResultExt: Sized {
    type Value;
    type Error: Into<Box<dyn std::error::Error>>;

    fn into_result(self) -> Result<Self::Value, Self::Error>;
    fn internal(self) -> Result<Self::Value, RequestError> {
        self.into_result().map_err(|error| {
            error!("Internal server error: {:?}", &*error.into());
            RequestError::Internal
        })
    }

    fn client(self, code: u16, message: &'static str) -> Result<Self::Value, RequestError> {
        let code = StatusCode::from_u16(code).expect("Invalid HTTP status code");
        self.into_result().map_err(move |error| {
            let error = error.into();
            debug!("Client error: {:?}", &*error);
            RequestError::Client { code, message, error: Some(error), }
        })
    }
}

impl<T, E: Into<Box<dyn std::error::Error>>> ResultExt for Result<T, E> {
    type Value = T;
    type Error = E;

    fn into_result(self) -> Result<Self::Value, Self::Error> {
        self
    }
}

#[derive(Copy, Clone, Debug, Deserialize)]
enum Direction {
    Buy,
    Sell,
}

#[derive(Deserialize)]
struct HedgeRequest {
    direction: Direction,
    amount: f64,
    currency: String,
    target_currency: String,
    fee: f64,
    sat_fee_percentage: f64,
}

#[derive(Clone)]
struct Handler {
    exchange: Arc<dyn HedgeProvider + Send + Sync>,
    conversion_table: Arc<Mutex<ConversionTable>>,
}

async fn handle_web_req(handler: Handler, req: Request<Body>) -> Result<Response<Body>, RequestError> {
    let root_path = "";
    let path = req.uri().path();
    if !path.starts_with(root_path) {
        return Err(RequestError::not_found());
    }

    let path = &path[root_path.len()..];

    match (req.method(), path) {
        (&Method::GET, "/") => {
            let index = std::fs::read(Path::new(STATIC_DIR).join("index.html")).internal()?;
            Ok(Response::new(Body::from(index)))
        },
        (&Method::POST, "/hedge") => {
            let mut conversion_table = handler.conversion_table.lock().await;
            let price = handler.exchange.fetch_price().await.internal()?;
            conversion_table.update_btc_price(dbg!(price));

            let bytes = hyper::body::to_bytes(req.into_body()).await?;
            let request = serde_json::from_slice::<HedgeRequest>(&bytes).client(400, "invalid request")?;
            let currency = conversion_table.decode_currency(&request.currency).ok_or(RequestError::simple(400, "invalid currency"))?;
            let target_currency = conversion_table.decode_currency(&request.target_currency).ok_or(RequestError::simple(400, "invalid target currency"))?;
            let amount = Amount {
                value: request.amount,
                currency,
            };


            let to_hedge = compute_hedge_amount(amount, request.fee, SatEarnPercentage::new(request.sat_fee_percentage), &conversion_table, request.direction);

            let hedged = handler.exchange.hedge(to_hedge.value).await.internal()?;
            let client_to_vekslak = match (request.direction, currency) {
                (Direction::Sell, _) => amount.value,
                (Direction::Buy, Currency::Sats) => conversion_table.from_native(amount.value / 100_000_000.0 * hedged.hedge_price, target_currency).value * (1.0 + request.fee),
                (Direction::Buy, Currency::GovShitcoin(_)) => conversion_table.to_native(amount).value / hedged.hedge_price * (1.0 + request.fee),
            };

            let vekslak_to_client = match (request.direction, currency) {
                (Direction::Buy, _) => amount.value,
                (Direction::Sell, Currency::Sats) => conversion_table.from_native(amount.value / 100_000_000.0 * hedged.hedge_price, target_currency).value * (1.0 - request.fee),
                (Direction::Sell, Currency::GovShitcoin(_)) => conversion_table.to_native(amount).value / hedged.hedge_price * (1.0 - request.fee),
            };

            let response = HedgeResponse {
                hedged_amount: to_hedge.value,
                hedge_price: hedged.hedge_price,
                liquidation_price: hedged.liquidation_price,
                client_to_vekslak, 
                vekslak_to_client,
            };

            Ok(Response::new(Body::from(serde_json::to_string(&response).internal()?)))
        },
        _ => Err(RequestError::not_found()),
    }
}

#[derive(Deserialize)]
struct Config {
    exchange: ExchangeConfig,
}

#[derive(Deserialize)]
struct ExchangeConfig {
    bybit: ByBitConfig,
}

#[derive(Deserialize)]
struct ByBitConfig {
    api_key: String,
    api_secret: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let mut conversion_table = ConversionTable::new(50_000.0, "USD".to_owned());
    conversion_table.register_shitcoin(1.132, "EUR".to_owned());
    conversion_table.register_shitcoin(0.044138, "CZK".to_owned());

    let mut config_path = dirs_next::config_dir().ok_or("Unknown config dir")?;
    config_path.push("vekslosh/config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    let config = toml::from_str::<Config>(&config)?;

    let provider = ByBitProvider {
        api_key: config.exchange.bybit.api_key,
        api_secret: config.exchange.bybit.api_secret,
        base: "https://api.bybit.com",
    };

    let handler = Handler {
        exchange: Arc::new(provider),
        conversion_table: Arc::new(Mutex::new(conversion_table)),
    };

    let service = make_service_fn(move |_| {
        let handler = handler.clone();

        async move {
            Ok::<_, hyper::Error>(service_fn(move |request| {
                let handler = handler.clone();
                async move {
                    handle_web_req(handler, request).await
                        .or_else(RequestError::into_response)
                }
            }))
        }
    });

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3001));
    let server = Server::bind(&addr).serve(service);

    println!("Listening on http://{}", addr);

    server.await?;

    Ok(())
}

trait HedgeProvider {
    fn hedge(&self, amount: f64) -> PinBoxFuture<Hedged, Box<dyn std::error::Error>>;
    fn fetch_price(&self) -> PinBoxFuture<f64, Box<dyn std::error::Error>>;
}

struct MockProvider;

impl HedgeProvider for MockProvider {
    fn hedge(&self, _amount: f64) -> PinBoxFuture<Hedged, Box<dyn std::error::Error>> {
        Box::pin(async {
            Ok(Hedged {
                hedge_price: 50000.0,
                liquidation_price: 40000.0,
            })
        })
    }

    fn fetch_price(&self) -> PinBoxFuture<f64, Box<dyn std::error::Error>> {
        Box::pin(async {
            Ok(50000.0)
        })
    }
}

struct ByBitProvider {
    api_key: String,
    api_secret: String,
    base: &'static str,
}

impl HedgeProvider for ByBitProvider {
    fn hedge(&self, mut amount: f64) -> PinBoxFuture<Hedged, Box<dyn std::error::Error>> {
        use std::fmt::Write;

        assert!(!amount.is_nan());
        assert!(amount != 0.0);
        amount = amount.round();
        let side = if amount > 0.0 {
            "Buy"
        } else {
            amount *= -1.0;
            "Sell"
        };
        let qty = amount as u64;

        let timestamp = std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_millis();
        let request_data = format!("api_key={}&order_type=Market&qty={}&side={}&symbol=BTCUSD&time_in_force=GoodTillCancel&timestamp={}", self.api_key, qty, side, timestamp);
        let hmac = hmac_sha256::HMAC::mac(request_data.as_bytes(), self.api_secret.as_bytes());
        let mut hmac_hex = String::with_capacity(64);
        for b in &hmac {
            write!(&mut hmac_hex, "{:02x}", b).unwrap();
        }

        let url = format!("{}/v2/private/order/create?{}&sign={}", self.base, request_data, hmac_hex);
        info!("Hedging, url: {}", url);

        let api_key = self.api_key.clone();
        let api_secret = self.api_secret.clone();

        let base = self.base;

        Box::pin(async move {
            let response = reqwest::Client::new()
                .post(url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(request_data)
                .send()
                .await?;
            info!("hedging http status: {}", response.status());
            let response = response
                .json::<ByBitOrderResponse>()
                .await?;

            let ret_msg = response.ret_msg;
            let result = response.result.ok_or_else(move || ret_msg.unwrap_or_else(|| "failed to hedge".to_owned()))?;

            let request_data2 = format!("api_key={}&order_id={}&symbol=BTCUSD&timestamp={}", api_key, result.order_id, timestamp);
            let hmac2 = hmac_sha256::HMAC::mac(request_data2.as_bytes(), api_secret.as_bytes());
            let mut hmac_hex2 = String::with_capacity(64);
            for b in &hmac2 {
                write!(&mut hmac_hex2, "{:02x}", b).unwrap();
            }
            let url2 = format!("{}/v2/private/order?{}&sign={}", base, request_data2, hmac_hex2);
            info!("Info, url: {}", url2);

            let mut to_sleep = 500;

            let price = loop {
                tokio::time::sleep(std::time::Duration::from_millis(to_sleep)).await;

                let response2 = reqwest::Client::new()
                    .get(&url2)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(request_data2.clone())
                    .send()
                    .await?;
                info!("hedging http status: {}", response2.status());
                let response2 = response2
                    .json::<ByBitOrderResponse>()
                    .await?;

                if let Some(result) = response2.result {
                    if result.last_exec_price != 0.0 {
                        break result.last_exec_price;
                    }
                } else {
                    return Err("failed to get hedge last executed price".into());
                }

                if to_sleep < 60000 {
                    to_sleep *= 2;
                }
            };

            info!("Hedged {} at price {}, order id: {}, last exec price: {}", amount, price, result.order_id, result.last_exec_price);
            Ok(Hedged {
                hedge_price: price,
                liquidation_price: 0.0,
            })
        })
    }

    fn fetch_price(&self) -> PinBoxFuture<f64, Box<dyn std::error::Error>> {
        let url = format!("{}/v2/public/tickers", self.base);
        Box::pin(async move {
            let response = reqwest::get(&url)
                .await?
                .json::<ByBitTickerResponse>()
                .await?;
            let item = response.result.into_iter().find(|item| item.symbol == "BTCUSD").ok_or("Ticker for BTCUSD not found")?;
            item.last_price.parse::<f64>().map_err(Into::into)
        })
    }
}

//{"api_key":"{api_key}","side":"Buy","symbol":"BTCUSD","order_type":"Market","qty":10,"time_in_force":"GoodTillCancel","timestamp":{timestamp},"sign":"{sign}"}
#[derive(Serialize)]
struct ByBitOrderRequest<'a> {
    api_key: &'a str,
    side: &'a str,
    symbol: &'a str,
    order_type: &'a str,
    qty: u64,
    time_in_force: &'a str,
    timestamp: u64,
}

impl<'a> ByBitOrderRequest<'a> {
    fn new(api_key: &'a str, mut value: f64) -> Self {
        assert!(!value.is_nan());
        assert!(value != 0.0);
        value = value.round();
        let side = if value > 0.0 {
            "Buy"
        } else {
            value *= -1.0;
            "Sell"
        };

        let timestamp = std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs();
        ByBitOrderRequest {
            api_key,
            side,
            symbol: "BTCUSB",
            order_type: "Market",
            qty: value as u64,
            time_in_force: "GoodTillCancel",
            timestamp,
        }
    }
}

#[derive(Deserialize)]
struct ByBitOrderResponse {
    result: Option<ByBitOrderResult>,
    ret_msg: Option<String>,
}

#[derive(Deserialize)]
struct ByBitOrderResult {
    #[serde(deserialize_with = "deserialize_maybe_stringly_f64")]
    price: f64,
    order_id: String,
    #[serde(deserialize_with = "deserialize_maybe_stringly_f64")]
    last_exec_price: f64,
}

#[derive(Deserialize)]
struct ByBitTickerResponse {
    result: Vec<ByBitTickerItem>,
}

#[derive(Deserialize)]
struct ByBitTickerItem {
    symbol: String,
    last_price: String,
}

fn deserialize_maybe_stringly_f64<'de, D>(deserializer: D) -> Result<f64, D::Error> where D: serde::Deserializer<'de> {
    struct Visitor;

    impl<'a> serde::de::Visitor<'a> for Visitor {
        type Value = f64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("bare or stringly-encoded float or integer")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> where E: serde::de::Error {
            Ok(v as f64)
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> where E: serde::de::Error {
            Ok(v)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> where E: serde::de::Error {
            v.parse().map_err(|error| E::invalid_value(serde::de::Unexpected::Str(v), &"bare or stringly-encoded float or integer"))
        }
    }

    deserializer.deserialize_any(Visitor)
}
