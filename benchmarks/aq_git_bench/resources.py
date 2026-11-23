import csv
import os
import pathlib
import statistics
import threading
import time


def percentile(values, p):
    if not values:
        return None
    return sorted(values)[min(len(values) - 1, int((len(values) - 1) * p))]


def summarize(samples, elapsed, errors):
    lat = [x[1] for x in samples]
    return {
        "successful_queries": len(lat),
        "errors": errors,
        "seconds": elapsed,
        "queries_per_second": len(lat) / elapsed,
        "p50_ms": percentile(lat, 0.5),
        "p95_ms": percentile(lat, 0.95),
        "p99_ms": percentile(lat, 0.99),
        "classes": {
            op: {
                "count": len(v),
                "queries_per_second": len(v) / elapsed,
                "p50_ms": percentile(v, 0.5),
                "p95_ms": percentile(v, 0.95),
                "p99_ms": percentile(v, 0.99),
            }
            for op in ("inspect", "contains", "tree", "diff", "batch")
            if (v := [x[1] for x in samples if x[0] == op])
        },
    }


class ResourceMonitor:
    def __init__(self, raw_path, service_pid, background):
        self.raw_path = raw_path
        self.service_pid = service_pid
        self.background = background
        self.stop = threading.Event()
        self.summary = {"peak_sampled_rss_kib_sum": 0, "samples": 0}
        self.rss_samples = []
        self.thread = threading.Thread(target=self.run) if service_pid else None

    def start(self):
        if self.thread:
            self.thread.start()

    def run(self):
        raw_path = self.raw_path
        monitor_stop = self.stop
        resource_summary = self.summary
        rss_samples = self.rss_samples
        with open(str(raw_path) + ".resources.csv", "w", newline="") as stream:
            writer = csv.writer(stream)
            writer.writerow(
                ["monotonic_seconds", "rss_kib_sum", "process_count", "cpu_seconds_sum"]
            )
            while not monitor_stop.is_set():
                trees = [
                    process_tree(pid)
                    for pid in [
                        self.service_pid,
                        *getattr(self.background, "additional_pids", []),
                    ]
                ]
                tree = {
                    "current_rss_kib_sum": sum(
                        (t["current_rss_kib_sum"] for t in trees)
                    ),
                    "processes": [p for t in trees for p in t["processes"]],
                }
                rss = tree["current_rss_kib_sum"]
                rss_samples.append(rss)
                resource_summary["peak_sampled_rss_kib_sum"] = max(
                    resource_summary["peak_sampled_rss_kib_sum"], rss
                )
                resource_summary["samples"] += 1
                writer.writerow(
                    [
                        time.perf_counter(),
                        rss,
                        len(tree["processes"]),
                        sum(
                            (
                                p["user_seconds"] + p["system_seconds"]
                                for p in tree["processes"]
                            )
                        ),
                    ]
                )
                monitor_stop.wait(0.1)

    def finish(self):
        self.stop.set()
        if self.thread:
            self.thread.join()
        if self.rss_samples:
            width = max(1, len(self.rss_samples) // 10)
            self.summary.update(
                first_rss_kib_sum=self.rss_samples[0],
                last_rss_kib_sum=self.rss_samples[-1],
                steady_rss_drift_kib=statistics.mean(self.rss_samples[-width:])
                - statistics.mean(
                    self.rss_samples[-2 * width : -width] or self.rss_samples[:width]
                ),
            )
        return self.summary


def process_tree(pid):
    records = []
    pending = [pid]
    while pending:
        current = pending.pop()
        try:
            status = pathlib.Path(f"/proc/{current}/status").read_text()
            stat = pathlib.Path(f"/proc/{current}/stat").read_text()
            fields = stat[stat.rfind(")") + 2 :].split()
            memory = {
                line.split(":", 1)[0]: line.split(":", 1)[1].strip()
                for line in status.splitlines()
                if line.startswith(("Name:", "VmRSS:", "VmHWM:"))
            }
            records.append(
                {
                    "pid": current,
                    **memory,
                    "user_seconds": int(fields[11]) / os.sysconf("SC_CLK_TCK"),
                    "system_seconds": int(fields[12]) / os.sysconf("SC_CLK_TCK"),
                }
            )
            pending.extend(
                (
                    int(v)
                    for v in pathlib.Path(f"/proc/{current}/task/{current}/children")
                    .read_text()
                    .split()
                )
            )
        except (FileNotFoundError, ProcessLookupError):
            pass
    return {
        "processes": records,
        "current_rss_kib_sum": sum(
            (int(p.get("VmRSS", "0 kB").split()[0]) for p in records)
        ),
        "note": "Per-process VmHWM is reported; sum of individual high-water marks is not simultaneous peak RSS.",
    }
