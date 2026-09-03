//! Weather lookup MCP server using Open-Meteo API.
use std::io::{self, BufRead, Write};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize)]
struct GeoResult {
    results: Option<Vec<Location>>,
}

#[derive(Deserialize, Clone)]
struct Location {
    name: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct WeatherResult {
    current_weather: CurrentWeather,
}

#[derive(Deserialize)]
struct CurrentWeather {
    temperature: f64,
    weathercode: u8,
}

fn fetch_location(city: &str) -> Result<Location> {
    let url = format!("https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=en&format=json", city);
    let resp: GeoResult = reqwest::blocking::get(&url)
        .context("Failed to connect to geocoding API")?
        .json()
        .context("Failed to parse geocoding response")?;
    
    match resp.results.and_then(|mut r| r.pop()) {
        Some(loc) => Ok(loc),
        None => bail!("City not found: {}", city),
    }
}

fn fetch_weather(loc: &Location) -> Result<CurrentWeather> {
    let url = format!("https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&current_weather=true", loc.latitude, loc.longitude);
    let resp: WeatherResult = reqwest::blocking::get(&url)
        .context("Failed to connect to weather API")?
        .json()
        .context("Failed to parse weather response")?;
    
    Ok(resp.current_weather)
}

fn handle_call(params: &Value) -> Result<Value> {
    let name = params.get("name").and_then(|v| v.as_str());
    if name != Some("weather") {
        bail!("Unknown tool");
    }
    
    let args = params.get("arguments").ok_or_else(|| anyhow::anyhow!("Missing arguments"))?;
    let city = args.get("city").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("Missing 'city' argument"))?;
    
    let loc = fetch_location(city)?;
    let weather = fetch_weather(&loc)?;
    
    let desc = match weather.weathercode {
        0 => "Clear sky",
        1..=3 => "Partly cloudy",
        45 | 48 => "Fog",
        51..=55 => "Drizzle",
        61..=65 => "Rain",
        71..=75 => "Snow",
        95..=99 => "Thunderstorm",
        _ => "Unknown weather",
    };
    
    let output = format!("Current weather in {}: {}°C, {}", loc.name, weather.temperature, desc);
    Ok(json!({
        "content": [{"type": "text", "text": output}],
        "isError": false
    }))
}

fn main() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut line = String::new();

    while reader.read_line(&mut line)? > 0 {
        if line.trim().is_empty() {
            line.clear();
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            line.clear();
            continue;
        };

        if let Some(obj) = value.as_object() {
            if let Some(method) = obj.get("method").and_then(|m| m.as_str()) {
                let id = obj.get("id").unwrap_or(&Value::Null);
                
                let response = match method {
                    "server/discover" => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2026-07-28",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "crumb-weather-mcp", "version": "1.0.0"}
                            }
                        })
                    }
                    "initialize" => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-11-25",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "crumb-weather-mcp", "version": "1.0.0"}
                            }
                        })
                    }
                    "ping" => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {}
                        })
                    }
                    "tools/list" => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [
                                    {
                                        "name": "weather",
                                        "description": "Look up current weather for a city",
                                        "inputSchema": {
                                            "type": "object",
                                            "properties": {
                                                "city": { "type": "string" }
                                            },
                                            "required": ["city"]
                                        },
                                        "annotations": {
                                            "readOnlyHint": true,
                                            "openWorldHint": true
                                        }
                                    }
                                ]
                            }
                        })
                    }
                    "tools/call" => {
                        let params = obj.get("params").unwrap_or(&Value::Null);
                        match handle_call(params) {
                            Ok(result) => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": result
                            }),
                            Err(e) => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "content": [{"type": "text", "text": e.to_string()}],
                                    "isError": true
                                }
                            })
                        }
                    }
                    _ => {
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {"code": -32601, "message": "method not found"}
                        })
                    }
                };

                if id != &Value::Null {
                    if let Ok(mut encoded) = serde_json::to_vec(&response) {
                        encoded.push(b'\n');
                        let _ = stdout.write_all(&encoded);
                        let _ = stdout.flush();
                    }
                }
            }
        }
        line.clear();
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_call_missing_city() {
        let params = json!({
            "name": "weather",
            "arguments": {}
        });
        let result = handle_call(&params);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Missing 'city' argument");
    }
}
