#!/usr/bin/env python3
from io import BytesIO
import json
import time

from char_framework.char_test_framework import CharTestFramework
from char_framework.char_util import str_to_hex
from test_framework.blocktools import COINBASE_MATURITY
from test_framework.util import assert_equal
from test_framework.messages import (
    deser_compact_size,
    ser_compact_size,
    ser_varint,
    deser_varint,
)

def encode_referendum_vote(ballot_number: int, payload_hex: str) -> str:
    """
    LeafType::REFERENDUM_VOTE == 0
    [1 byte leaf_type][varint ballot_number][compact_size payload_len][payload]
    """
    payload = bytes.fromhex(payload_hex)
    buf = BytesIO()
    buf.write(b"\x00")  # leaf type
    buf.write(ser_varint(ballot_number))
    buf.write(ser_compact_size(len(payload)))
    buf.write(payload)
    return buf.getvalue().hex()


def decode_referendum_vote_hex(vote_hex: str) -> tuple[int, bytes]:
    raw = BytesIO(bytes.fromhex(vote_hex))
    leaf_type = raw.read(1)[0]
    assert_equal(leaf_type, 0)
    ballot = deser_varint(raw)
    payload_len = deser_compact_size(raw)
    payload = raw.read(payload_len)
    return ballot, payload


# =====================================================================
# Plebfi Demo
# =====================================================================

class CharPlebfiDemo(CharTestFramework):
    def skip_test_if_missing_module(self):
        self.skip_if_no_wallet()

    def setup_network(self):
        self.setup_nodes()

    def set_test_params(self):
        super().set_test_params()

    def run_test(self):
        # -----------------------------------------------------------------
        # STEP 1: Start nodes, connect, and mine maturity
        # -----------------------------------------------------------------
        self.log.info("STEP 1: bootstrap chain")
        self.connect_nodes(0, 1)
        self.connect_nodes(1, 0)

        self.generate_and_wait(self.nodes[0], COINBASE_MATURITY + 1)
        assert_equal(self.nodes[0].getblockcount(), COINBASE_MATURITY + 1)
        self.log.info("Chain height: %d", self.nodes[0].getblockcount())

        # # # -----------------------------------------------------------------
        # # # STEP 2: Create CHAR bond
        # # # -----------------------------------------------------------------
        self.log.info("STEP 2: create CHAR bond")
        bond = self.nodes[0].walletcreatetaprootoutputforcharbond(1)
        self.log.info("CHAR bond: %s", json.dumps(bond, indent=2, default=str))

        # Mine bond into a block
        self.generate_and_wait(self.nodes[0], 1)

        rawtx = bond["hex"]
        decodedtx = self.nodes[0].decoderawtransaction(rawtx)
        self.log.info(
            "Decoded CHAR bond transaction:\n%s",
            json.dumps(decodedtx, indent=2, default=str),
        )

        # Bond activates after 6 blocks
        self.generate_and_wait(self.nodes[0], 6)

        allcharbonds = self.nodes[0].getallcharbonds()
        assert_equal(len(allcharbonds), 1)
        self.log.info(
            "  - All CHAR bonds:\n%s",
            json.dumps(allcharbonds, indent=2, default=str),
        )

        # # # -----------------------------------------------------------------
        # # # STEP 3: Register and schedule the app
        # # # -----------------------------------------------------------------
        self.log.info("STEP 3: add app to app registry and schedule it")
        app_preimage = str_to_hex("plebfi-demo-domain")

        add_app = self.nodes[0].add_app_to_app_registry(
            app_preimage,
            "Plebfi Demo App",
        )
        assert add_app["success"]
        self.log.info(
            "add_app_to_app_registry result:\n%s",
            json.dumps(add_app, indent=2, default=str),
        )

        schedule_app = self.nodes[0].config_app_registry_schedule(app_preimage, True)
        assert schedule_app["success"]
        self.log.info(
            "config_app_registry_schedule result:\n%s",
            json.dumps(schedule_app, indent=2, default=str),
        )

        # # # NOTE: Any app in the tracker attests every second.
        # # # You can also call `atttestbonds` RPC if you want to force an attestation.

        # # # -----------------------------------------------------------------
        # # # STEP 4: Send referendum vote for ballot 0
        # # # -----------------------------------------------------------------
        self.log.info("STEP 4: send referendum vote for ballot 0")

        vote0_payload = str_to_hex("helloplebfi")
        vote0_hex = encode_referendum_vote(0, vote0_payload)
        res = self.nodes[0].addbambookv([{app_preimage: vote0_hex}], False)

        self.log.info(
            "addbambookv result:\n%s",
            json.dumps(res, indent=2, default=str),
        )
        assert res[app_preimage]

        # # -----------------------------------------------------------------
        # # STEP 5: Check for decision roll for ballot 0
        # # -----------------------------------------------------------------
        # self.log.info("STEP 5: wait for decision roll for ballot 0")

        # # Decision roll should NOT be found initially
        initial_roll = self.nodes[0].getreferendumdecisionroll(app_preimage, 0, 0)
        assert_equal(initial_roll["found"], False)
        self.log.info(
            "  - decision roll (should NOT be found yet):\n%s",
            json.dumps(initial_roll, indent=2, default=str),
        )

        # # # -----------------------------------------------------------------
        # # # STEP 5.1: Wait for decision roll for ballot 0
        # # # -----------------------------------------------------------------

        # Wait until found
        def synced():
            roll_result = self.nodes[0].getreferendumdecisionroll(app_preimage, 0, 0)
            return roll_result["found"]

        self.wait_until(synced, timeout=3)

        found_roll = self.nodes[0].getreferendumdecisionroll(app_preimage, 0, 2)
        assert found_roll["found"]
        assert_equal(found_roll["ballot_number"], 0)

        roll_details = found_roll["decision_roll"]
        ballot_from_data, payload_bytes = decode_referendum_vote_hex(
            roll_details["data"]
        )

        assert_equal(ballot_from_data, 0)
        assert_equal(payload_bytes.hex(), vote0_payload)

        self.log.info(
            "decision roll (found):\n%s",
            json.dumps(found_roll, indent=2, default=str),
        )

        # # -----------------------------------------------------------------
        # # STEP 6: Mini polling app (ballots 1 to 5)
        # # -----------------------------------------------------------------
        self.log.info("STEP 6: mini polling app for ballots 1..5")

        for ballot_num in range(1, 6):
            self.log.info("  - processing ballot %d", ballot_num)
            voted = False

            while True:
                # NOTE: Check status, if not found, and leader, submit vote, else if found go to next ballot
                status = self.nodes[0].getreferendumdecisionroll(
                    app_preimage, ballot_num, 0
                )

                if status["found"]:
                    self.log.info(
                        "decision roll %d confirmed found, moving to next.",
                        ballot_num,
                    )
                    break

                if status["leader_is_mine"] and not voted:
                    self.log.info(
                        "ballot %d: I am leader, submitting vote...",
                        ballot_num,
                    )

                    loop_payload = str_to_hex(f"vote_for_ballot_{ballot_num}")
                    loop_vote_hex = encode_referendum_vote(ballot_num, loop_payload)

                    res = self.nodes[0].addbambookv(
                        [{app_preimage: loop_vote_hex}],
                        False,
                    )
                    assert res[app_preimage]
                    voted = True

                time.sleep(0.1)

        self.log.info("Success: reached ballot 5.")

        # # # -----------------------------------------------------------------
        # # # STEP 7: Verify second node has all decision rolls
        # # # -----------------------------------------------------------------
        # self.log.info("STEP 7: verify node 1 synced all decision rolls")

        def synced():
            roll_result = self.nodes[1].getreferendumdecisionroll(app_preimage, 5, 0)
            return roll_result["found"]

        self.wait_until(synced, timeout=3)

        second_node_roll = self.nodes[1].getreferendumdecisionroll(app_preimage, 5, 2)
        self.log.info(
            "node1 ballot 5 roll:\n%s",
            json.dumps(second_node_roll, indent=2, default=str),
        )
        assert second_node_roll["found"]

        self.log.info("node1 has all decision rolls up to ballot 5.")


if __name__ == "__main__":
    CharPlebfiDemo(__file__).main()
