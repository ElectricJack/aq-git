import argparse
import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]


def parse_args(description):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--profile", choices=["smoke", "reference"], default="smoke")
    parser.add_argument(
        "--scenarios",
        default="read,batch,cold,agent-loop,mixed,churn,overload,workflow",
    )
    parser.add_argument("--seconds", type=float)
    parser.add_argument("--repetitions", type=int)
    parser.add_argument("--churn-seconds", type=float)
    parser.add_argument("--mixed-seconds", type=float)
    parser.add_argument("--aged-batch-followup", action="store_true")
    parser.add_argument("--clients", default="1,8,32")
    parser.add_argument("--backends", default="service,git_process,git_persistent")
    parser.add_argument("--arrival-rate", type=float, default=0)
    parser.add_argument(
        "--workdir", type=pathlib.Path, default=pathlib.Path("/tmp/aq-git-benchmark")
    )
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument(
        "--binary", type=pathlib.Path, default=ROOT / "target/release/aq-git"
    )
    args = parser.parse_args()
    validate_args(args, parser)
    return args, parser


def nonpositive(value):
    return value is not None and value <= 0


def validate_args(args, parser):
    if nonpositive(args.seconds):
        parser.error("seconds must be positive")
    if nonpositive(args.repetitions):
        parser.error("repetitions must be positive")
    if any(nonpositive(value) for value in (args.churn_seconds, args.mixed_seconds)):
        parser.error("scenario durations must be positive")
    if args.arrival_rate < 0:
        parser.error("arrival rate must be nonnegative")
    if any(not 1 <= int(v) <= 128 for v in args.clients.split(",")):
        parser.error("clients must be in 1..128")


class BenchmarkPlan:
    def __init__(self, args, parser):
        self.args = args
        self.manifest = json.loads(
            (ROOT / "benchmarks/fixtures/reference.json").read_text()
        )
        self.reference = self.args.profile == "reference"
        self.set_durations(args)
        self.commits = self.manifest["history"]["commits"] if self.reference else 2000
        self.refs = self.manifest["history"]["refs"] if self.reference else 200
        self.keys = self.manifest["workload"]["distinct_warm_keys"]
        self.clients = [int(v) for v in self.args.clients.split(",")]
        self.backends = tuple(self.args.backends.split(","))
        self.scenarios = args.scenarios.split(",")
        validate_choices(
            self.backends,
            ("service", "git_process", "git_persistent"),
            "backend",
            parser,
        )
        validate_choices(
            self.scenarios,
            (
                "read",
                "batch",
                "cold",
                "agent-loop",
                "mixed",
                "churn",
                "overload",
                "workflow",
            ),
            "scenario",
            parser,
        )

    def set_durations(self, args):
        default_seconds = 60 if self.reference else 2
        self.seconds = args.seconds if args.seconds is not None else default_seconds
        self.repetitions = args.repetitions or (5 if self.reference else 1)
        reference_churn = self.reference and args.seconds is None
        default_churn = 1800 if reference_churn else self.seconds
        self.churn_seconds = (
            default_churn if args.churn_seconds is None else args.churn_seconds
        )
        self.mixed_seconds = (
            self.seconds if args.mixed_seconds is None else args.mixed_seconds
        )


def validate_choices(values, allowed, name, parser):
    if not set(values).issubset(allowed):
        parser.error(f"unknown {name}")
