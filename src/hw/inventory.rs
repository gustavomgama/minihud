//! The flattened LibreHardwareMonitor sensor inventory: every sensor the
//! bridge reports, one row per hardware node and sensor name.
//!
//! The bridge payload stays flat — `(hw, type, name, value)` — so this module
//! is where the flat dump becomes an inventory that keeps every sensor
//! distinct. Two jobs live here:
//!
//! * [`disambiguate`] — number the sensors that share a `(hw, type, name)`
//!   triple (`GpuNvidia:NVIDIA GeForce RTX 3070 #1`, `#2`) so two nodes LHM
//!   reports identically (two same-model GPUs, a second socket's SuperIO) never
//!   fold into one.
//! * [`render`] — print the whole inventory for the CLI dump flag, so coverage
//!   can be checked against LHM directly without the rendered overlay.

use std::collections::HashMap;

use super::lhm::LhmSensor;

/// Number the sensors that share one `(hw, type, name)` triple.
///
/// The flat payload carries no node identity, so a repeated triple is the only
/// signal that a second hardware node reports the same label: two same-model
/// GPUs each report a `Load GPU Core`, a second socket's SuperIO each reports a
/// `Fan #1`. Every occurrence is numbered (`… #1`, `… #2`, …) so no sensor is
/// dropped or overwritten. A triple that appears once keeps the plain label,
/// which leaves the curated matching in [`super::apply`] undisturbed.
pub(crate) fn disambiguate(rows: &[LhmSensor]) -> Vec<LhmSensor> {
    let mut totals: HashMap<(&str, &str, &str), usize> = HashMap::new();
    for r in rows {
        *totals
            .entry((r.hw.as_str(), r.sensor_type.as_str(), r.name.as_str()))
            .or_default() += 1;
    }
    let mut seen: HashMap<(&str, &str, &str), usize> = HashMap::new();
    rows.iter()
        .map(|r| {
            let key = (r.hw.as_str(), r.sensor_type.as_str(), r.name.as_str());
            let nth = seen.entry(key).or_insert(0);
            *nth += 1;
            let nth = *nth;
            let mut row = r.clone();
            if totals[&key] > 1 {
                row.hw = format!("{} #{nth}", r.hw);
            }
            row
        })
        .collect()
}

/// The hardware label without a collision suffix (`… #2`), so a curated lookup
/// keeps matching every row of the node it picked — the first row of a
/// colliding node is itself suffixed, and matching that exact label would hide
/// the node's other readings.
pub(crate) fn node_label(hw: &str) -> &str {
    match hw.rsplit_once(" #") {
        Some((base, n)) if !base.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
        _ => hw,
    }
}

/// Render the whole inventory for the `--dump-inventory` flag: a header, then
/// one block per hardware label with every sensor in bridge order. A node whose
/// sensors repeat a name reports one block per suffixed sensor, because each
/// such sensor is its own row in the flat payload.
pub fn render(sensors: &[LhmSensor]) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "{} sensors across {} nodes\n",
        sensors.len(),
        blocks(sensors)
    );
    let mut label: Option<&str> = None;
    for s in sensors {
        if label != Some(s.hw.as_str()) {
            let _ = writeln!(out, "{}", s.hw);
            label = Some(s.hw.as_str());
        }
        let _ = writeln!(out, "  {} {} = {}", s.sensor_type, s.name, s.value);
    }
    out
}

/// How many hardware labels the inventory spans (one block each in `render`).
fn blocks(sensors: &[LhmSensor]) -> usize {
    sensors
        .iter()
        .fold((0, None), |(n, prev): (usize, Option<&str>), s| {
            if prev == Some(s.hw.as_str()) {
                (n, prev)
            } else {
                (n + 1, Some(s.hw.as_str()))
            }
        })
        .0
}

/// A hand-written dump covering every LHM hardware category the bridge opens:
/// CPU, NVIDIA + Intel GPUs, memory, motherboard (+ a SuperIO subnode), an NVMe
/// drive, network, battery, fan controllers and a PSU.
#[cfg(test)]
const FULL_INVENTORY: &str = r#"[
  {"hw":"Cpu:Intel Core i7-12700H","type":"Load","name":"CPU Total","value":42.0},
  {"hw":"Cpu:Intel Core i7-12700H","type":"Temperature","name":"CPU Package","value":61.5},
  {"hw":"Cpu:Intel Core i7-12700H","type":"Power","name":"Package","value":28.25},
  {"hw":"Cpu:Intel Core i7-12700H","type":"Clock","name":"CPU Core #1","value":2400.0},
  {"hw":"Cpu:Intel Core i7-12700H","type":"Voltage","name":"Core #1 VID","value":1.55},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Temperature","name":"GPU Core","value":67.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"GPU Core","value":79.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"GPU Power","value":64.946},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Power","name":"GPU Package","value":164.572},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Clock","name":"GPU Core","value":1920.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Clock","name":"GPU Memory","value":7800.994},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Fan","name":"GPU Fan 1","value":1333.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Control","name":"GPU Fan 1","value":31.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"SmallData","name":"GPU Memory Total","value":8192.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"SmallData","name":"GPU Memory Used","value":7729.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"SmallData","name":"D3D Dedicated Memory Used","value":7565.637},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Throughput","name":"GPU PCIe Rx","value":901832700.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"D3D Copy","value":0.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"D3D Copy","value":0.7164105},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"D3D Copy","value":0.0},
  {"hw":"GpuIntel:Intel UHD Graphics","type":"Load","name":"GPU Core","value":5.0},
  {"hw":"GpuIntel:Intel UHD Graphics","type":"Temperature","name":"GPU Core","value":44.0},
  {"hw":"Memory:Generic Memory","type":"Data","name":"Memory Used","value":8.0},
  {"hw":"Memory:Generic Memory","type":"Data","name":"Memory Available","value":24.0},
  {"hw":"Motherboard:ASUS ROG STRIX B550-F","type":"Temperature","name":"Temperature #1","value":36.0},
  {"hw":"Motherboard:ASUS ROG STRIX B550-F/SuperIO","type":"Fan","name":"Fan #1","value":1200.0},
  {"hw":"Motherboard:ASUS ROG STRIX B550-F/SuperIO","type":"Voltage","name":"Vcore","value":1.2},
  {"hw":"Storage:NVMe Samsung SSD 980 PRO 1TB","type":"Temperature","name":"Temperature","value":45.0},
  {"hw":"Storage:NVMe Samsung SSD 980 PRO 1TB","type":"Load","name":"Used Space","value":71.5},
  {"hw":"Storage:NVMe Samsung SSD 980 PRO 1TB","type":"Data","name":"Available Spare","value":98.0},
  {"hw":"Storage:NVMe Samsung SSD 980 PRO 1TB","type":"Throughput","name":"Read Rate","value":512.0},
  {"hw":"Network:Intel Ethernet Controller I225-V","type":"Load","name":"Network Utilization","value":3.0},
  {"hw":"Network:Intel Ethernet Controller I225-V","type":"Data","name":"Bytes Sent","value":1024.0},
  {"hw":"Network:Intel Ethernet Controller I225-V","type":"Data","name":"Bytes Received","value":2048.0},
  {"hw":"Network:Intel Ethernet Controller I225-V","type":"Data","name":"Upload Speed","value":512.0},
  {"hw":"Battery:Primary","type":"Voltage","name":"Voltage","value":12.4},
  {"hw":"Battery:Primary","type":"Charge","name":"Charge Level","value":88.0},
  {"hw":"Battery:Primary","type":"Temperature","name":"Temperature","value":31.0},
  {"hw":"Cooler:NZXT Kraken X63","type":"Load","name":"Load","value":12.0},
  {"hw":"Cooler:NZXT Kraken X63","type":"Fan","name":"Fan","value":1500.0},
  {"hw":"Cooler:NZXT Kraken X63","type":"Control","name":"Control","value":40.0},
  {"hw":"Psu:Corsair RM850x","type":"Power","name":"Power","value":220.0},
  {"hw":"Psu:Corsair RM850x","type":"Voltage","name":"+12V","value":12.1}
]"#;

/// Two same-model GPUs, each reporting the same two sensors.
#[cfg(test)]
const TWO_MODEL_GPUS: &str = r#"[
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Temperature","name":"GPU Core","value":67.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"GPU Core","value":79.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Temperature","name":"GPU Core","value":63.0},
  {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"GPU Core","value":12.0}
]"#;

/// A multi-socket board: the same SuperIO subnode label twice.
#[cfg(test)]
const DUPLICATE_SUBNODES: &str = r#"[
  {"hw":"Motherboard:ASUS PRO WS WRX80E-SAGE/SuperIO","type":"Fan","name":"Fan #1","value":1100.0},
  {"hw":"Motherboard:ASUS PRO WS WRX80E-SAGE/SuperIO","type":"Fan","name":"Fan #1","value":1150.0}
]"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge dump through the production path: parse the flat JSON, then
    /// number the colliding sensors.
    fn dump(json: &str) -> Vec<LhmSensor> {
        let rows: Vec<LhmSensor> = serde_json::from_str(json).expect("a valid bridge dump");
        disambiguate(&rows)
    }

    #[test]
    fn a_full_inventory_dump_parses_with_every_sensor_present() {
        // Catches: the feed dropping rows (or whole hardware categories) once
        // the bridge reports the complete inventory instead of the four
        // curated blocks — sensors would silently vanish from the app.
        let rows = dump(FULL_INVENTORY);
        assert_eq!(rows.len(), 43, "every row of the dump must survive");
        let labels: Vec<&str> = rows.iter().map(|s| s.hw.as_str()).collect();
        for label in [
            "Cpu:Intel Core i7-12700H",
            "GpuNvidia:NVIDIA GeForce RTX 3070",
            "GpuIntel:Intel UHD Graphics",
            "Memory:Generic Memory",
            "Motherboard:ASUS ROG STRIX B550-F",
            "Motherboard:ASUS ROG STRIX B550-F/SuperIO",
            "Storage:NVMe Samsung SSD 980 PRO 1TB",
            "Network:Intel Ethernet Controller I225-V",
            "Battery:Primary",
            "Cooler:NZXT Kraken X63",
            "Psu:Corsair RM850x",
        ] {
            assert!(
                labels.iter().any(|l| node_label(l) == label),
                "category {label} missing from the parsed inventory"
            );
        }
        let gpu_load = rows
            .iter()
            .find(|s| {
                node_label(&s.hw) == "GpuNvidia:NVIDIA GeForce RTX 3070"
                    && s.sensor_type == "Load"
                    && s.name == "GPU Core"
            })
            .expect("the curated GPU load sensor must keep its plain label");
        assert_eq!(gpu_load.hw, "GpuNvidia:NVIDIA GeForce RTX 3070");
        assert_eq!(gpu_load.value, 79.0);
        let nvme = rows
            .iter()
            .find(|s| {
                node_label(&s.hw) == "Storage:NVMe Samsung SSD 980 PRO 1TB"
                    && s.sensor_type == "Temperature"
            })
            .expect("the NVMe temperature sensor must be parsed");
        assert_eq!(nvme.value, 45.0);
    }

    #[test]
    fn two_same_model_gpus_each_keep_every_sensor_under_their_own_label() {
        // Catches: two nodes LHM reports with the same label collapsing into
        // one, so the second GPU's temperature and load are indistinguishable
        // from (or lost beside) the first's.
        let rows = dump(TWO_MODEL_GPUS);
        assert_eq!(rows.len(), 4, "no sensor may be dropped");
        let labels: Vec<&str> = rows.iter().map(|s| s.hw.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "GpuNvidia:NVIDIA GeForce RTX 3070 #1",
                "GpuNvidia:NVIDIA GeForce RTX 3070 #1",
                "GpuNvidia:NVIDIA GeForce RTX 3070 #2",
                "GpuNvidia:NVIDIA GeForce RTX 3070 #2",
            ]
        );
        assert_eq!(
            (rows[0].value, rows[2].value),
            (67.0, 63.0),
            "each node keeps its own readings"
        );
    }

    #[test]
    fn repeated_sensor_names_inside_one_node_are_numbered_not_merged() {
        // Catches: LHM reporting several sensors with the same name (the D3D
        // engine counters) — the rows collide on (hw, type, name), so they must
        // be numbered rather than treated as one sensor or dropped as a
        // duplicate. The curated lookup still resolves, because the plain label
        // resolves to the node's first row of each name.
        let rows = dump(
            r#"[
              {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"D3D Copy","value":0.0},
              {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"D3D Copy","value":0.7164105},
              {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Load","name":"D3D Copy","value":0.0},
              {"hw":"GpuNvidia:NVIDIA GeForce RTX 3070","type":"Clock","name":"D3D Copy","value":100.0}
            ]"#,
        );
        assert_eq!(rows.len(), 4, "no sensor may be dropped");
        let labels: Vec<&str> = rows.iter().map(|s| s.hw.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "GpuNvidia:NVIDIA GeForce RTX 3070 #1",
                "GpuNvidia:NVIDIA GeForce RTX 3070 #2",
                "GpuNvidia:NVIDIA GeForce RTX 3070 #3",
                "GpuNvidia:NVIDIA GeForce RTX 3070",
            ],
            "a different sensor type is its own sensor and keeps the plain label"
        );
    }

    #[test]
    fn duplicated_subnodes_across_a_multi_socket_board_are_suffixed() {
        // Catches: a multi-socket board's repeated SuperIO collapsing into one
        // node, losing the second socket's fans and voltages.
        let rows = dump(DUPLICATE_SUBNODES);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].hw, "Motherboard:ASUS PRO WS WRX80E-SAGE/SuperIO #1");
        assert_eq!(rows[1].hw, "Motherboard:ASUS PRO WS WRX80E-SAGE/SuperIO #2");
        assert_eq!((rows[0].value, rows[1].value), (1100.0, 1150.0));
    }

    #[test]
    fn node_label_strips_only_a_numeric_collision_suffix() {
        // Catches: a real hardware name containing " #" being truncated, so the
        // curated matching would look at a label no sensor has.
        assert_eq!(node_label("GpuNvidia:RTX 3070 #2"), "GpuNvidia:RTX 3070");
        assert_eq!(node_label("Storage:NVMe #1"), "Storage:NVMe");
        assert_eq!(
            node_label("Network:Ethernet #12"),
            "Network:Ethernet",
            "multi-digit suffixes are stripped"
        );
        assert_eq!(
            node_label("Cpu:Intel Core i7 #12x"),
            "Cpu:Intel Core i7 #12x",
            "a non-numeric suffix is part of the name"
        );
        assert_eq!(
            node_label("Motherboard:ASUS ROG/SuperIO"),
            "Motherboard:ASUS ROG/SuperIO",
            "a plain label is unchanged"
        );
    }

    #[test]
    fn render_prints_every_sensor_grouped_by_label() {
        // Catches: a dump flag that prints only the curated rows, so the
        // complete inventory cannot be reviewed from the CLI.
        let rows = dump(FULL_INVENTORY);
        let text = render(&rows);
        assert!(
            text.starts_with("43 sensors across 14 nodes"),
            "the header must count the inventory: {text}"
        );
        assert_eq!(
            text.lines().filter(|l| l.starts_with("  ")).count(),
            43,
            "one line per sensor: {text}"
        );
        for s in &rows {
            assert!(text.contains(&s.hw), "label {s:?} missing from the dump");
            assert!(text.contains(&s.name), "name {s:?} missing from the dump");
        }
        assert!(
            text.contains("Load CPU Total = 42"),
            "each line carries type, name and value: {text}"
        );
        assert!(
            text.contains("Throughput GPU PCIe Rx = 901832700"),
            "large readings must print in full: {text}"
        );
        assert!(
            text.contains("GpuNvidia:NVIDIA GeForce RTX 3070 #2"),
            "suffixed sensors appear under their own label: {text}"
        );
    }

    #[test]
    fn render_of_an_empty_inventory_is_a_zero_header() {
        // Catches: an empty dump rendering a misleading header (or panicking on
        // the empty slice) when the bridge has no sensors to report.
        assert_eq!(render(&[]), "0 sensors across 0 nodes\n");
    }
}
