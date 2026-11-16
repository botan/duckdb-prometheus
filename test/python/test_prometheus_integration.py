#!/usr/bin/env python3
"""End-to-end tests against an ephemeral Prometheus-compatible HTTP server."""

from __future__ import annotations

import json
import re
import sys
import threading
import unittest
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import duckdb

EXTENSION_PATH: Path | None = None
LARGE_RESPONSE_PADDING_BYTES = 33 * 1_024 * 1_024
MULTI_CHUNK_ROW_COUNT = 2_051


class PrometheusHandler(BaseHTTPRequestHandler):
    requests: list[dict[str, object]] = []

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        parsed = urlsplit(self.path)
        parameters = parse_qs(parsed.query, keep_blank_values=True)
        self.requests.append(
            {
                "path": parsed.path,
                "parameters": parameters,
                "headers": {key.lower(): value for key, value in self.headers.items()},
            }
        )

        handlers = {
            "/proxy/api/v1/query_range": self._handle_range,
            "/proxy/api/v1/query": self._handle_instant,
            "/proxy/api/v1/labels": self._handle_labels,
            "/proxy/api/v1/label/job/values": self._handle_label_values,
            "/proxy/api/v1/label/U__http_2e_status/values": (
                self._handle_utf8_label_values
            ),
            "/proxy/api/v1/series": self._handle_series,
            "/proxy/api/v1/metadata": self._handle_metadata,
        }
        handler = handlers.get(parsed.path)
        if handler is None:
            self._json_response(404, {"status": "error", "error": "wrong path"})
            return
        for name in ("start", "end", "time"):
            for value in parameters.get(name, []):
                if re.fullmatch(r"-?[0-9]+(?:\.[0-9]+)?", value) is None:
                    self._json_response(
                        400, {"status": "error", "error": "nonnumeric timestamp"}
                    )
                    return
        handler(parameters)

    def _handle_range(self, parameters: dict[str, list[str]]) -> None:
        if not all(name in parameters for name in ("query", "start", "end", "step")):
            self._json_response(400, {"status": "error", "error": "missing parameter"})
            return
        query = parameters["query"][0]

        if query == "error_query":
            self._json_response(
                200,
                {
                    "status": "error",
                    "errorType": "bad_data",
                    "error": "invalid integration query",
                },
            )
        elif query == "control_error_query":
            self._json_response(
                200,
                {
                    "status": "error",
                    "errorType": "bad\0data",
                    "error": "first\nsecond",
                },
            )
        elif query == "warning_query":
            self._json_response(
                200,
                {
                    "status": "success",
                    "warnings": ["store gateway unavailable"],
                    "data": {
                        "resultType": "matrix",
                        "result": [{"metric": {}, "values": [[1784541600, "7"]]}],
                    },
                },
            )
        elif query == "empty_query":
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {"resultType": "matrix", "result": []},
                },
            )
        elif query == "unlabeled_query":
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {
                        "resultType": "matrix",
                        "result": [{"metric": {}, "values": [[1784541600, "1"]]}],
                    },
                },
            )
        elif query in {"histogram_query", "mixed_histogram_query"}:
            series: dict[str, object] = {
                "metric": {"__name__": "request_duration_seconds"},
                "histograms": [
                    [1784541600, {"count": "1", "sum": "0.5", "buckets": []}]
                ],
            }
            if query == "mixed_histogram_query":
                series["values"] = [[1784541660, "0.5"]]
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {"resultType": "matrix", "result": [series]},
                },
            )
        elif query == "large_response_query":
            self._large_success_response()
        elif query == "chunk_query":
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {
                        "resultType": "matrix",
                        "result": [
                            {
                                "metric": {"__name__": "chunk_metric"},
                                "values": [
                                    [1784541600 + index, str(index)]
                                    for index in range(2_048)
                                ],
                            },
                            {
                                "metric": {
                                    "__name__": "chunk_metric",
                                    "job": "api",
                                },
                                "values": [
                                    [1784541600 + index, str(index)]
                                    for index in range(2_048, MULTI_CHUNK_ROW_COUNT)
                                ],
                            },
                        ],
                    },
                },
            )
        else:
            expected = {
                "query": ['node_cpu_seconds_total{mode="idle"}'],
                "start": ["1784541600"],
                "end": ["1784545200"],
                "step": ["60"],
            }
            if parameters != expected:
                self._json_response(400, {"status": "error", "error": "bad request"})
                return
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {
                        "resultType": "matrix",
                        "result": [
                            {
                                "metric": {
                                    "mode": "idle",
                                    "instance": "host-b",
                                    "__name__": "node_cpu_seconds_total",
                                    "job": "api",
                                },
                                "values": [
                                    [1784541600.0, "1"],
                                    [1784541660.5, "2.5"],
                                ],
                            },
                            {
                                "metric": {
                                    "instance": "host-a",
                                    "__name__": "node_cpu_seconds_total",
                                    "mode": "idle",
                                },
                                "values": [[1784541600.0, "-1"]],
                            },
                        ],
                    },
                },
            )

    def _handle_instant(self, parameters: dict[str, list[str]]) -> None:
        query = parameters.get("query", [None])[0]
        if query == "instant_vector":
            self._json_response(
                200,
                {
                    "status": "success",
                    "warnings": ["partial response"],
                    "data": {
                        "resultType": "vector",
                        "result": [
                            {
                                "metric": {"__name__": "up", "job": "api"},
                                "value": [1784541600, "1"],
                            }
                        ],
                    },
                },
            )
        elif query == "instant_empty":
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {"resultType": "vector", "result": []},
                },
            )
        elif query == "instant_scalar":
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {"resultType": "scalar", "result": [1784541600, "2"]},
                },
            )
        elif query == "instant_string":
            self._json_response(
                200,
                {
                    "status": "success",
                    "data": {"resultType": "string", "result": [1784541600, "ok"]},
                },
            )
        else:
            self._json_response(400, {"status": "error", "error": "bad query"})

    def _handle_labels(self, _parameters: dict[str, list[str]]) -> None:
        self._json_response(
            200,
            {"status": "success", "data": ["__name__", "instance", "job"]},
        )

    def _handle_label_values(self, _parameters: dict[str, list[str]]) -> None:
        self._json_response(
            200,
            {"status": "success", "data": ["api", "prometheus"]},
        )

    def _handle_utf8_label_values(self, _parameters: dict[str, list[str]]) -> None:
        self._json_response(200, {"status": "success", "data": ["200", "500"]})

    def _handle_series(self, _parameters: dict[str, list[str]]) -> None:
        self._json_response(
            200,
            {
                "status": "success",
                "data": [
                    {"job": "api", "__name__": "up", "instance": "host-a"},
                    {"job": "api", "__name__": "up", "instance": "host-b"},
                ],
            },
        )

    def _handle_metadata(self, parameters: dict[str, list[str]]) -> None:
        if parameters.get("metric") not in (None, ["http_requests_total"]):
            self._json_response(400, {"status": "error", "error": "bad metric"})
            return
        self._json_response(
            200,
            {
                "status": "success",
                "data": {
                    "http_requests_total": [
                        {
                            "type": "counter",
                            "help": "Number of HTTP requests",
                            "unit": "requests",
                        },
                        {
                            "type": "counter",
                            "help": "Amount of HTTP requests",
                            "unit": "requests",
                        },
                    ]
                },
            },
        )

    def _json_response(self, status: int, body: dict[str, object]) -> None:
        encoded = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def _large_success_response(self) -> None:
        prefix = b'{"status":"success","padding":"'
        suffix = b'","data":{"resultType":"matrix","result":[]}}'
        padding_length = LARGE_RESPONSE_PADDING_BYTES
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(prefix) + padding_length + len(suffix)))
        self.end_headers()
        self.wfile.write(prefix)
        block = b"x" * (1_024 * 1_024)
        for _ in range(padding_length // len(block)):
            self.wfile.write(block)
        self.wfile.write(suffix)

    def log_message(self, _format: str, *_args: object) -> None:
        pass


class PrometheusIntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if EXTENSION_PATH is None:
            raise RuntimeError("extension path was not configured")
        extension = EXTENSION_PATH.resolve()
        if not extension.is_file():
            raise RuntimeError(f"extension does not exist: {extension}")

        PrometheusHandler.requests.clear()
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), PrometheusHandler)
        cls.server_thread = threading.Thread(
            target=cls.server.serve_forever, daemon=True
        )
        cls.server_thread.start()
        cls.endpoint = f"http://127.0.0.1:{cls.server.server_port}/proxy"
        cls.connection = duckdb.connect(config={"allow_unsigned_extensions": "true"})
        escaped_extension = str(extension).replace("'", "''")
        cls.connection.execute(f"LOAD '{escaped_extension}'")

    @classmethod
    def tearDownClass(cls) -> None:
        cls.connection.close()
        cls.server.shutdown()
        cls.server.server_close()
        cls.server_thread.join(timeout=5)

    def test_matrix_rows_query_first_parameters_and_types(self) -> None:
        rows = self.connection.execute(
            """
            SELECT labels, epoch_us(timestamp), value
            FROM prometheus_scan(
                ?, ?, ?,
                endpoint := ?,
                step := ?,
                request_timeout := ?
            )
            ORDER BY labels['instance'], timestamp
            """,
            [
                'node_cpu_seconds_total{mode="idle"}',
                datetime(2026, 7, 20, 10, tzinfo=timezone.utc),
                datetime(2026, 7, 20, 11, tzinfo=timezone.utc),
                self.endpoint,
                timedelta(minutes=1),
                timedelta(seconds=5),
            ],
        ).fetchall()

        self.assertEqual(len(rows), 3)
        self.assertEqual(
            rows[0],
            (
                {
                    "__name__": "node_cpu_seconds_total",
                    "instance": "host-a",
                    "mode": "idle",
                },
                1_784_541_600_000_000,
                -1.0,
            ),
        )
        self.assertEqual(
            rows[1],
            (
                {
                    "__name__": "node_cpu_seconds_total",
                    "instance": "host-b",
                    "job": "api",
                    "mode": "idle",
                },
                1_784_541_600_000_000,
                1.0,
            ),
        )
        self.assertEqual(rows[2][1], 1_784_541_660_500_000)
        self.assertEqual(rows[2][2], 2.5)
        self.assertEqual(list(rows[0][0]), ["__name__", "instance", "mode"])
        self.assertNotIn("authorization", PrometheusHandler.requests[-1]["headers"])

    def test_promoted_labels_are_columns_and_missing_values_are_null(self) -> None:
        rows = self.connection.execute(
            """
            SELECT instance, job, labels['mode']
            FROM prometheus_scan(
                'node_cpu_seconds_total{mode="idle"}',
                '2026-07-20 10:00:00+00',
                '2026-07-20 11:00:00+00',
                endpoint := ?,
                labels := ['instance', 'job']
            )
            ORDER BY instance, timestamp
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(
            rows,
            [
                ("host-a", None, "idle"),
                ("host-b", "api", "idle"),
                ("host-b", "api", "idle"),
            ],
        )

    def test_query_failures_surface_as_duckdb_errors(self) -> None:
        for query, expected in (
            ("error_query", "bad_data.*invalid integration query"),
            ("control_error_query", r"bad\\u\{0\}data.*first\\nsecond"),
            ("histogram_query", "native histogram samples.*not supported"),
            ("mixed_histogram_query", "native histogram samples.*not supported"),
        ):
            with self.subTest(query=query):
                with self.assertRaisesRegex(duckdb.Error, expected):
                    self.connection.execute(
                        """
                        SELECT * FROM prometheus_scan(
                            ?,
                            '2026-07-20 10:00:00+00',
                            '2026-07-20 11:00:00+00',
                            endpoint := ?
                        )
                        """,
                        [query, self.endpoint],
                    ).fetchall()

    def test_warnings_and_large_success_responses_are_accepted(self) -> None:
        warning_rows = self.connection.execute(
            """
            SELECT value FROM prometheus_scan(
                'warning_query',
                '2026-07-20 10:00:00+00',
                '2026-07-20 11:00:00+00',
                endpoint := ?
            )
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(warning_rows, [(7.0,)])

        large_count = self.connection.execute(
            """
            SELECT count(*) FROM prometheus_scan(
                'large_response_query',
                '2026-07-20 10:00:00+00',
                '2026-07-20 11:00:00+00',
                endpoint := ?
            )
            """,
            [self.endpoint],
        ).fetchone()[0]
        self.assertEqual(large_count, 0)

    def test_defaults_empty_results_and_explicit_bounds(self) -> None:
        count = self.connection.execute(
            """
            SELECT count(*) FROM prometheus_scan(
                'empty_query',
                '1969-12-31T23:59:59.5Z',
                '2026-07-20T10:00:00.000600Z',
                endpoint := ?
            )
            """,
            [self.endpoint],
        ).fetchone()[0]
        self.assertEqual(count, 0)
        parameters = PrometheusHandler.requests[-1]["parameters"]
        self.assertEqual(parameters["start"], ["-0.5"])
        self.assertEqual(parameters["end"], ["1784541600.0006"])
        self.assertEqual(parameters["step"], ["60"])

        labels = self.connection.execute(
            """
            SELECT labels FROM prometheus_scan(
                'unlabeled_query',
                '2026-07-20 10:00:00',
                '2026-07-20 11:00:00',
                endpoint := ?
            )
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(labels, [({},)])

    def test_projection_pushdown_across_multiple_vectors(self) -> None:
        count = self.connection.execute(
            """
            SELECT count(*) FROM prometheus_scan(
                'chunk_query',
                '2026-07-20 10:00:00+00',
                '2026-07-20 11:00:00+00',
                endpoint := ?
            )
            """,
            [self.endpoint],
        ).fetchone()[0]
        self.assertEqual(count, MULTI_CHUNK_ROW_COUNT)

        rows = self.connection.execute(
            """
            SELECT value, job
            FROM prometheus_scan(
                'chunk_query',
                '2026-07-20 10:00:00+00',
                '2026-07-20 11:00:00+00',
                endpoint := ?,
                labels := ['job']
            )
            ORDER BY value
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(len(rows), MULTI_CHUNK_ROW_COUNT)
        self.assertEqual(rows[0], (0.0, None))
        self.assertEqual(rows[2_047], (2_047.0, None))
        self.assertEqual(rows[2_048], (2_048.0, "api"))
        self.assertEqual(rows[-1], (2_050.0, "api"))

    def test_instant_query_vector_scalar_and_unsupported_string(self) -> None:
        vector = self.connection.execute(
            """
            SELECT job, epoch_us(timestamp), value
            FROM prometheus_query(
                'instant_vector',
                endpoint := ?,
                evaluation_time := '2026-07-20T10:00:00.999600Z',
                labels := ['job']
            )
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(vector, [("api", 1_784_541_600_000_000, 1.0)])
        parameters = PrometheusHandler.requests[-1]["parameters"]
        self.assertEqual(parameters["time"], ["1784541600.9996"])

        scalar = self.connection.execute(
            """
            SELECT labels, value
            FROM prometheus_query('instant_scalar', endpoint := ?)
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(scalar, [({}, 2.0)])
        self.assertNotIn("time", PrometheusHandler.requests[-1]["parameters"])

        with self.assertRaisesRegex(duckdb.Error, "unsupported resultType.*string"):
            self.connection.execute(
                "SELECT * FROM prometheus_query('instant_string', endpoint := ?)",
                [self.endpoint],
            ).fetchall()

    def test_scan_many_uses_sequential_union_all_semantics(self) -> None:
        request_offset = len(PrometheusHandler.requests)
        rows = self.connection.execute(
            """
            SELECT value, job
            FROM prometheus_scan_many(
                ['empty_query', 'warning_query', 'unlabeled_query', 'warning_query'],
                '2026-07-20 10:00:00+00',
                '2026-07-20 11:00:00+00',
                endpoint := ?,
                labels := ['job']
            )
            """,
            [self.endpoint],
        ).fetchall()

        self.assertEqual(rows, [(7.0, None), (1.0, None), (7.0, None)])
        for request in PrometheusHandler.requests[request_offset:]:
            self.assertEqual(request["parameters"]["start"], ["1784541600"])
            self.assertEqual(request["parameters"]["end"], ["1784545200"])
        queries = [
            request["parameters"]["query"][0]
            for request in PrometheusHandler.requests[request_offset:]
        ]
        self.assertEqual(
            queries,
            ["empty_query", "warning_query", "unlabeled_query", "warning_query"],
        )

    def test_query_many_skips_empty_results_and_surfaces_later_failures(self) -> None:
        request_offset = len(PrometheusHandler.requests)
        rows = self.connection.execute(
            """
            SELECT labels['job'], value
            FROM prometheus_query_many(
                ['instant_empty', 'instant_vector', 'instant_scalar', 'instant_vector'],
                endpoint := ?,
                evaluation_time := '2026-07-20 10:00:00+00'
            )
            """,
            [self.endpoint],
        ).fetchall()

        self.assertEqual(rows, [("api", 1.0), (None, 2.0), ("api", 1.0)])
        for request in PrometheusHandler.requests[request_offset:]:
            self.assertEqual(request["parameters"]["time"], ["1784541600"])
        queries = [
            request["parameters"]["query"][0]
            for request in PrometheusHandler.requests[request_offset:]
        ]
        self.assertEqual(
            queries,
            ["instant_empty", "instant_vector", "instant_scalar", "instant_vector"],
        )

        request_offset = len(PrometheusHandler.requests)
        with self.assertRaisesRegex(duckdb.Error, "unsupported resultType.*string"):
            self.connection.execute(
                """
                SELECT *
                FROM prometheus_query_many(
                    ['instant_vector', 'instant_string'],
                    endpoint := ?
                )
                """,
                [self.endpoint],
            ).fetchall()
        for request in PrometheusHandler.requests[request_offset:]:
            self.assertNotIn("time", request["parameters"])

    def test_discovery_helpers_and_parameters(self) -> None:
        labels = self.connection.execute(
            """
            SELECT label
            FROM prometheus_labels(
                endpoint := ?,
                matcher := '{job="api"}',
                start_time := '2026-07-20 10:00:00+00',
                end_time := '2026-07-20 11:00:00+00'
            )
            ORDER BY label
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(labels, [("__name__",), ("instance",), ("job",)])
        parameters = PrometheusHandler.requests[-1]["parameters"]
        self.assertEqual(parameters["match[]"], ['{job="api"}'])
        self.assertEqual(parameters["start"], ["1784541600"])
        self.assertEqual(parameters["end"], ["1784545200"])

        values = self.connection.execute(
            """
            SELECT value
            FROM prometheus_label_values(
                'job', endpoint := ?, start_time := '1969-12-31T23:59:59.5Z'
            )
            ORDER BY value
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(values, [("api",), ("prometheus",)])
        parameters = PrometheusHandler.requests[-1]["parameters"]
        self.assertEqual(parameters["start"], ["-0.5"])
        self.assertNotIn("end", parameters)

        utf8_values = self.connection.execute(
            """
            SELECT value
            FROM prometheus_label_values('http.status', endpoint := ?)
            ORDER BY value
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(utf8_values, [("200",), ("500",)])
        parameters = PrometheusHandler.requests[-1]["parameters"]
        self.assertNotIn("start", parameters)
        self.assertNotIn("end", parameters)

        series = self.connection.execute(
            """
            SELECT labels
            FROM prometheus_series(
                '{job="api"}', endpoint := ?,
                end_time := '2026-07-20T10:00:00.000600Z'
            )
            ORDER BY labels['instance']
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(series[0][0]["instance"], "host-a")
        self.assertEqual(series[1][0]["instance"], "host-b")
        parameters = PrometheusHandler.requests[-1]["parameters"]
        self.assertEqual(parameters["match[]"], ['{job="api"}'])
        self.assertNotIn("start", parameters)
        self.assertEqual(parameters["end"], ["1784541600.0006"])

    def test_metadata_helper_and_projection(self) -> None:
        rows = self.connection.execute(
            """
            SELECT metric, type, help, unit
            FROM prometheus_metadata(
                endpoint := ?,
                metric := 'http_requests_total'
            )
            ORDER BY help
            """,
            [self.endpoint],
        ).fetchall()
        self.assertEqual(
            rows,
            [
                (
                    "http_requests_total",
                    "counter",
                    "Amount of HTTP requests",
                    "requests",
                ),
                (
                    "http_requests_total",
                    "counter",
                    "Number of HTTP requests",
                    "requests",
                ),
            ],
        )
        count = self.connection.execute(
            "SELECT count(*) FROM prometheus_metadata(endpoint := ?)",
            [self.endpoint],
        ).fetchone()[0]
        self.assertEqual(count, 2)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: test_prometheus_integration.py EXTENSION_PATH")
    EXTENSION_PATH = Path(sys.argv.pop())
    unittest.main()
