#!/bin/bash
set -u

SESSION="char_demo"
DATA_DIR="/Users/stu/char"
BITCOIN_BIN="${DATA_DIR}/char-bitcoin/build/bin"

# --- CONFIGURATION ---
CHAR_DNS_APP_ID="17f24e073d4eb05ef1779b2ea4c9895e7ad0d25e08d188ef5818aa9e1112f5ef"

NODE1_DIR="${DATA_DIR}/char_data/node1"
NODE2_DIR="${DATA_DIR}/char_data/node2"
NODE3_DIR="${DATA_DIR}/char_data/node3"

NODE1_RPC=18443
NODE2_RPC=18453
NODE3_RPC=18463

NODE1_P2P=18444
NODE2_P2P=18445
NODE3_P2P=18446

ZMQ1="tcp://127.0.0.1:28332"
ZMQ2="tcp://127.0.0.1:28333"
ZMQ3="tcp://127.0.0.1:28334"

BITCOIND_COMMON="-regtest -char_node -charbondindex -fallbackfee=0.00001 -maxtxfee=1000 -acceptnonstdtxn=1 -server=1 -txindex=1 -rpcbind=0.0.0.0 -rpcallowip=0.0.0.0/0 -debug=zmq"

log() { echo -e "\033[1;32m[SETUP]\033[0m $1"; }
error() { echo -e "\033[1;31m[ERROR]\033[0m $1"; }

cli() {
  local rpcport="$1"; shift
  local datadir="$1"; shift
  "${BITCOIN_BIN}/bitcoin-cli" -regtest -rpcport="${rpcport}" -datadir="${datadir}" "$@"
}

setup_node_pane() {
    local pane_id="$1"
    local node_name="$2"
    local cmd="$3"
    local rpc_port="$4"
    local data_dir="$5"

    # 1. Start the daemon
    tmux send-keys -t "$pane_id" "$cmd" C-m

    # 2. Wait briefly for startup
    sleep 2

    # 3. Fail-Fast Check
    if ! lsof -i :$rpc_port >/dev/null 2>&1; then
        error "$node_name failed to start!"
        echo "----------------------------------------------------"
        tmux capture-pane -pt "${SESSION}:${pane_id}"
        echo "----------------------------------------------------"
        tmux kill-session -t "${SESSION}"
        exit 1
    else
        log "$node_name started successfully."

        tmux send-keys -t "$pane_id" "cd '${BITCOIN_BIN}'" C-m

        tmux send-keys -t "$pane_id" "alias cli='./bitcoin-cli -regtest -rpcport=${rpc_port} -datadir=${data_dir}'" C-m

        tmux send-keys -t "$pane_id" "clear" C-m
        tmux send-keys -t "$pane_id" "echo -e '\033[1;34m=== ${node_name} READY ===\033[0m'" C-m
        tmux send-keys -t "$pane_id" "echo 'Directory: ${BITCOIN_BIN}'" C-m
        tmux send-keys -t "$pane_id" "echo 'Alias set: type [cli getblockchaininfo] to test'" C-m
    fi
}

wait_for_sync() {
  log "Syncing nodes..."
  local retries=20
  while [ $retries -gt 0 ]; do
    local h1=$(cli "${NODE1_RPC}" "${NODE1_DIR}" getblockcount)
    local h2=$(cli "${NODE2_RPC}" "${NODE2_DIR}" getblockcount)
    local h3=$(cli "${NODE3_RPC}" "${NODE3_DIR}" getblockcount)
    if [ "$h1" == "$h2" ] && [ "$h2" == "$h3" ]; then
      log "All nodes synced at height: $h1"
      return 0
    fi
    sleep 1
    ((retries--))
  done
  error "Nodes failed to sync."
  exit 1
}

if [ ! -f "${BITCOIN_BIN}/bitcoind" ]; then
    error "Binary not found at ${BITCOIN_BIN}/bitcoind"
    exit 1
fi

# --- CLEANUP ---
log "Cleaning up..."
tmux kill-session -t "${SESSION}" 2>/dev/null || true
PORTS_TO_CLEAR=($NODE1_RPC $NODE2_RPC $NODE3_RPC $NODE1_P2P $NODE2_P2P $NODE3_P2P)
for port in "${PORTS_TO_CLEAR[@]}"; do
    pid=$(lsof -ti :$port 2>/dev/null)
    if [ ! -z "$pid" ]; then kill -9 $pid 2>/dev/null || true; fi
done
sleep 2

rm -rf "${DATA_DIR}/char_data"
mkdir -p "${NODE1_DIR}" "${NODE2_DIR}" "${NODE3_DIR}"

# --- TMUX LAYOUT ---
tmux new-session -d -s "${SESSION}"
tmux set -g mouse on
tmux split-window -v -p 50
tmux split-window -h -p 66 -t 0
tmux split-window -h -p 50 -t 1
tmux split-window -h -p 66 -t 3
tmux split-window -h -p 50 -t 4

log "Starting Nodes (Daemon Mode)..."

# --- NODE 1 ---
CMD1="${BITCOIN_BIN}/bitcoind ${BITCOIND_COMMON} -datadir='${NODE1_DIR}' -port=${NODE1_P2P} -rpcport=${NODE1_RPC} -zmqpubleader='${ZMQ1}' -daemon"
setup_node_pane "0" "Node 1" "$CMD1" "$NODE1_RPC" "$NODE1_DIR"

# --- NODE 2 ---
CMD2="${BITCOIN_BIN}/bitcoind ${BITCOIND_COMMON} -datadir='${NODE2_DIR}' -port=${NODE2_P2P} -rpcport=${NODE2_RPC} -zmqpubleader='${ZMQ2}' -daemon"
setup_node_pane "1" "Node 2" "$CMD2" "$NODE2_RPC" "$NODE2_DIR"

# --- NODE 3 ---
CMD3="${BITCOIN_BIN}/bitcoind ${BITCOIND_COMMON} -datadir='${NODE3_DIR}' -port=${NODE3_P2P} -rpcport=${NODE3_RPC} -zmqpubleader='${ZMQ3}' -daemon"
setup_node_pane "2" "Node 3" "$CMD3" "$NODE3_RPC" "$NODE3_DIR"



log "Establishing Wallets..."
for node in "$NODE1_RPC $NODE1_DIR" "$NODE2_RPC $NODE2_DIR" "$NODE3_RPC $NODE3_DIR"; do
    read -r port dir <<< "$node"
    if ! cli "$port" "$dir" getwalletinfo >/dev/null 2>&1; then
        cli "$port" "$dir" createwallet "char" >/dev/null
    fi
done

log "Forcing P2P Connections..."
cli "${NODE2_RPC}" "${NODE2_DIR}" addnode "127.0.0.1:${NODE1_P2P}" "add" >/dev/null
cli "${NODE3_RPC}" "${NODE3_DIR}" addnode "127.0.0.1:${NODE1_P2P}" "add" >/dev/null
cli "${NODE3_RPC}" "${NODE3_DIR}" addnode "127.0.0.1:${NODE2_P2P}" "add" >/dev/null

log "Verifying Connections..."
sleep 2
conns=$(cli "${NODE2_RPC}" "${NODE2_DIR}" getnetworkinfo | grep '"connections":' | tr -dc '0-9')
if [ "${conns:-0}" -eq 0 ]; then
    error "Node 2 failed to connect to peers."
    exit 1
fi
log "P2P connections confirmed."

log "Mining initial blocks..."
ADDR1=$(cli "${NODE1_RPC}" "${NODE1_DIR}" getnewaddress)
cli "${NODE1_RPC}" "${NODE1_DIR}" generatetoaddress 101 "${ADDR1}" >/dev/null
wait_for_sync

ADDR2=$(cli "${NODE2_RPC}" "${NODE2_DIR}" getnewaddress)
cli "${NODE2_RPC}" "${NODE2_DIR}" generatetoaddress 101 "${ADDR2}" >/dev/null
wait_for_sync

ADDR3=$(cli "${NODE3_RPC}" "${NODE3_DIR}" getnewaddress)
cli "${NODE3_RPC}" "${NODE3_DIR}" generatetoaddress 101 "${ADDR3}" >/dev/null
wait_for_sync

log "Block Height: 303. Registering Apps..."

for node in "$NODE1_RPC $NODE1_DIR" "$NODE2_RPC $NODE2_DIR" "$NODE3_RPC $NODE3_DIR"; do
    read -r port dir <<< "$node"
    cli "$port" "$dir" add_app_to_app_registry "${CHAR_DNS_APP_ID}" "char_dns"
    cli "$port" "$dir" config_app_registry_schedule "${CHAR_DNS_APP_ID}" true
done

cli "${NODE1_RPC}" "${NODE1_DIR}" generatetoaddress 1 "${ADDR1}" >/dev/null
wait_for_sync

log "Creating Bonds..."
# Node 1
cli "${NODE1_RPC}" "${NODE1_DIR}" walletcreatetaprootoutputforcharbond 1
cli "${NODE1_RPC}" "${NODE1_DIR}" generatetoaddress 7 "${ADDR1}" >/dev/null
wait_for_sync
# Node 2
cli "${NODE2_RPC}" "${NODE2_DIR}" walletcreatetaprootoutputforcharbond 1
cli "${NODE2_RPC}" "${NODE2_DIR}" generatetoaddress 7 "${ADDR2}" >/dev/null
wait_for_sync
# Node 3
cli "${NODE3_RPC}" "${NODE3_DIR}" walletcreatetaprootoutputforcharbond 1
cli "${NODE3_RPC}" "${NODE3_DIR}" generatetoaddress 7 "${ADDR3}" >/dev/null
wait_for_sync

log "Starting Daemons..."
tmux send-keys -t 3 "export PDNS_API_KEY='supersecretkey'; cargo run -- --zmq '${ZMQ1}' --bitcoinrpc 'http://127.0.0.1:${NODE1_RPC}' --datadir '${NODE1_DIR}/regtest'" C-m
tmux send-keys -t 4 "export PDNS_API_KEY='supersecretkey'; cargo run -- --zmq '${ZMQ2}' --bitcoinrpc 'http://127.0.0.1:${NODE2_RPC}' --datadir '${NODE2_DIR}/regtest'" C-m
tmux send-keys -t 5 "export PDNS_API_KEY='supersecretkey'; cargo run -- --zmq '${ZMQ3}' --bitcoinrpc 'http://127.0.0.1:${NODE3_RPC}' --datadir '${NODE3_DIR}/regtest'" C-m

log "Setup Complete! Attaching..."
tmux attach -t "${SESSION}"
