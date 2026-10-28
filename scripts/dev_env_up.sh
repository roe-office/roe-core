#!/bin/bash
# 测试环境一键拉起: MySQL(Percona 9.7) + Hive Metastore [+ HiveServer2]
# 仅限本机测试库,账号密码为测试用途。用法: dev_env_up.sh [--hs2]
export JAVA_HOME=/usr/lib/jvm/java-8-openjdk-amd64
export HADOOP_HOME=$HOME/smalldata-lab/hadoop-3.3.6
export HIVE_HOME=$HOME/smalldata-lab/apache-hive-3.1.3-bin
export PATH=$JAVA_HOME/bin:$HADOOP_HOME/bin:$HIVE_HOME/bin:$PATH
LOGDIR=$HOME/smalldata-lab/logs
mkdir -p "$LOGDIR"
MYSQL_HIVE_PWD='Hive#2026change'

wait_port() { for i in $(seq 1 "${2:-30}"); do ss -ltn 2>/dev/null | grep -q ":$1\b" && return 0; sleep 1; done; return 1; }

echo "[1/3] MySQL"
sudo systemctl start mysqld
wait_port 3306 30 && echo "  3306 OK" || { echo "  3306 未监听,查看: sudo tail -30 /var/log/mysqld.log"; exit 1; }
MYSQL_PWD="$MYSQL_HIVE_PWD" mysql -uhive -h127.0.0.1 -e "select count(*) as metastore_tables from information_schema.tables where table_schema='hive_metastore'" 2>&1 | tail -2

echo "[2/3] Hive Metastore"
if ss -ltn | grep -q ":9083\b"; then echo "  9083 已在运行"; else
  cd "$HIVE_HOME" && nohup hive --service metastore > "$LOGDIR/metastore.log" 2>&1 &
  wait_port 9083 90 && echo "  9083 OK" || echo "  9083 未监听,查看: tail -40 $LOGDIR/metastore.log"
fi

if [ "$1" = "--hs2" ]; then
  echo "[3/3] HiveServer2"
  if ss -ltn | grep -q ":10000\b"; then echo "  10000 已在运行"; else
    cd "$HIVE_HOME" && nohup hive --service hiveserver2 > "$LOGDIR/hiveserver2.log" 2>&1 &
    wait_port 10000 120 && echo "  10000 OK" || echo "  10000 未监听,查看: tail -40 $LOGDIR/hiveserver2.log"
  fi
else
  echo "[3/3] HiveServer2 跳过(加 --hs2 启动)"
fi
echo "--- 端口 ---"; ss -ltn | grep -E ":(3306|9083|10000)\b"
