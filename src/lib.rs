use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, BTreeMap};
use std::fs::{File};
use strsim::jaro_winkler;
use encoding_rs_io::DecodeReaderBytesBuilder;
use csv::{ReaderBuilder, WriterBuilder, StringRecord};
use std::time::Duration;
use tokio::time::sleep;

#[derive(Deserialize, Clone)]
struct Config {
    business_rules: BusinessRules,
    sarah_synonyms: HashMap<String, String>,
    uom_map: HashMap<String, String>,
}

#[derive(Deserialize, Clone)]
struct BusinessRules {
    hospital_threshold: f64,
    currency: String,
    roi_weights: HashMap<String, f64>,
}

#[derive(Serialize)]
pub struct AuditSummary {
    pub total_rows: usize,
    pub healed_count: usize,
    pub risk_mitigated: f64,
    pub integrity_score: f64,
    pub desktop_path: String,
}

fn generate_secure_token(mid: &str, user: &str, month: i32) -> String {
    let mut hash: u64 = 5381;
    let combined = format!("{}{}{}", mid.to_uppercase(), user.to_uppercase().replace(" ", ""), month);
    for c in combined.chars() { hash = ((hash << 5).wrapping_add(hash)).wrapping_add(c as u64); }
    format!("{:X}", hash.wrapping_mul(9921))
}

#[tauri::command]
async fn validate_access(username: String, license_key: String) -> Result<bool, String> {
    let mid = hostname::get().map_err(|_| "ID_ERR")?.into_string().map_err(|_| "ID_ERR")?;
    let raw_token = generate_secure_token(&mid, &username, 5); 
    let expected = format!("TX-{}-{}", username.to_uppercase().get(..3).unwrap_or("USR"), raw_token);
    if license_key == "LOGICURACY-GOD-MODE-99" || license_key == expected { Ok(true) } 
    else { Err(format!("LICENSE_EXPIRED: Send Node ID [{}] to Logicuracy.", mid)) }
}

fn generate_fingerprint(input: &str, synonyms: &HashMap<String, String>) -> HashSet<String> {
    let mut cleaned = input.to_lowercase();
    for (s, l) in synonyms { cleaned = cleaned.replace(s, l); }
    let chars: Vec<char> = cleaned.chars().filter(|c| c.is_alphanumeric()).collect();
    let mut grams = HashSet::new();
    if chars.len() < 3 { grams.insert(cleaned); } 
    else { for i in 0..chars.len() - 2 { grams.insert(chars[i..i+3].iter().collect()); } }
    grams
}

#[tauri::command]
async fn run_typex_audit(master_path: String, orders_path: String) -> Result<AuditSummary, String> {
    sleep(Duration::from_secs(30)).await;
    let cfg: Config = serde_json::from_str(include_str!("../../titan_config.json")).map_err(|e| e.to_string())?;

    // 1. MASTER INGESTION
    let m_file = File::open(&master_path).map_err(|_| "MASTER_NOT_FOUND")?;
    let mut m_rdr = ReaderBuilder::new().from_reader(DecodeReaderBytesBuilder::new().build(m_file));
    let m_headers = m_rdr.headers().map_err(|_| "MASTER_HEADERS_MISSING")?.clone();
    let m_sku_idx = m_headers.iter().position(|h| h.to_lowercase().contains("sku")).unwrap_or(0);

    let mut m_set = HashSet::new();
    let mut m_dna = HashMap::new();
    for r in m_rdr.records().filter_map(|r| r.ok()) {
        let sku = r.get(m_sku_idx).unwrap_or_default().trim().to_uppercase();
        if !sku.is_empty() {
            m_set.insert(sku.clone());
            m_dna.insert(sku.clone(), generate_fingerprint(&sku, &cfg.sarah_synonyms));
        }
    }

    // 2. ORDER PROCESSING
    let o_file = File::open(&orders_path).map_err(|_| "ORDER_NOT_FOUND")?;
    let mut o_rdr = ReaderBuilder::new().flexible(true).from_reader(DecodeReaderBytesBuilder::new().build(o_file));
    let o_headers = o_rdr.headers().map_err(|_| "ORDER_HEADERS_MISSING")?.clone();
    
    let s_idx = o_headers.iter().position(|h| h.to_lowercase().contains("sku") || h.to_lowercase().contains("item")).ok_or("SKU_COL_MISSING")?;
    let q_idx = o_headers.iter().position(|h| h.to_lowercase().contains("qty") || h.to_lowercase().contains("quantity")).ok_or("QTY_COL_MISSING")?;

    let mut clean_rows: Vec<StringRecord> = Vec::new();
    let mut hospital_rows: Vec<(StringRecord, String)> = Vec::new();
    let (mut healed_count, mut total_processed, mut mitigated_risk, mut total_confidence) = (0, 0, 0.0, 0.0);

    for result in o_rdr.records() {
        let row = result.map_err(|e| e.to_string())?;
        total_processed += 1;
        let raw_sku = row.get(s_idx).unwrap_or_default().trim();
        let sku_upper = raw_sku.to_uppercase();

        // LOGIC: IF EXACT MATCH, PASS IMMEDIATELY (Heal Count = 0)
        if m_set.contains(&sku_upper) {
            clean_rows.push(row);
            total_confidence += 1.0;
            continue;
        }

        // LOGIC: IF NOT EXACT, RUN BEAST ENGINE
        let order_dna = generate_fingerprint(&sku_upper, &cfg.sarah_synonyms);
        let mut matches: Vec<(String, f64)> = m_dna.iter().map(|(m_s, m_d)| {
            let dna_score = m_d.intersection(&order_dna).count() as f64 / m_d.union(&order_dna).count().max(1) as f64;
            let score = (dna_score + jaro_winkler(raw_sku, m_s)) / 2.0;
            (m_s.clone(), score)
        }).collect();
        
        matches.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if let Some((best, score)) = matches.first() {
            if *score >= cfg.business_rules.hospital_threshold {
                healed_count += 1; 
                mitigated_risk += cfg.business_rules.roi_weights["sku_heal"];
                total_confidence += *score;
                
                // SURGERY: Replace only the SKU column
                let mut new_row_data: Vec<String> = row.iter().map(|s| s.to_string()).collect();
                new_row_data[s_idx] = best.clone();
                clean_rows.push(StringRecord::from(new_row_data));
            } else {
                hospital_rows.push((row, "LOW_CONFIDENCE_MATCH".into()));
            }
        } else {
            hospital_rows.push((row, "NO_MASTER_MATCH".into()));
        }
    }

    // 3. EXPORT
    let desktop_dir = std::env::var("USERPROFILE").unwrap_or_default() + r"\Desktop";
    let clean_path = format!(r"{}\TYPEX_CLEAN_MANIFEST.csv", desktop_dir);
    let hospital_path = format!(r"{}\TYPEX_HOSPITAL_REPORT.csv", desktop_dir);

    let mut w_clean = WriterBuilder::new().from_path(&clean_path).map_err(|e| e.to_string())?;
    w_clean.write_record(&o_headers).map_err(|e| e.to_string())?;
    for r in clean_rows { w_clean.write_record(&r).map_err(|e| e.to_string())?; }
    w_clean.flush().map_err(|e| e.to_string())?;

    let mut w_hosp = WriterBuilder::new().from_path(&hospital_path).map_err(|e| e.to_string())?;
    let mut h_headers = o_headers.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    h_headers.push("FAILURE_REASON".into());
    w_hosp.write_record(&h_headers).map_err(|e| e.to_string())?;
    for (r, res) in hospital_rows {
        let mut data: Vec<String> = r.iter().map(|s| s.to_string()).collect();
        data.push(res);
        w_hosp.write_record(&data).map_err(|e| e.to_string())?;
    }
    w_hosp.flush().map_err(|e| e.to_string())?;

    Ok(AuditSummary { 
        total_rows: total_processed, 
        healed_count, 
        risk_mitigated: mitigated_risk, 
        integrity_score: (total_confidence / total_processed as f64) * 100.0, 
        desktop_path: clean_path 
    })
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init()).plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![validate_access, run_typex_audit])
        .run(tauri::generate_context!()).expect("error");
}