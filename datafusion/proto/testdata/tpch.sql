-- Contains SQL statements for TPCH queries that get parsed and serialized

CREATE TABLE supplier (
        s_suppkey  INTEGER,
        s_name VARCHAR,
        s_address VARCHAR,
        s_nationkey INTEGER,
        s_phone VARCHAR,
        s_acctbal NUMERIC,
        s_comment VARCHAR,
);

CREATE TABLE part (
        p_partkey INTEGER,
        p_name VARCHAR,
        p_mfgr VARCHAR,
        p_brand VARCHAR,
        p_type VARCHAR,
        p_size INTEGER,
        p_container VARCHAR,
        p_retailprice NUMERIC,
        p_comment VARCHAR,
);

CREATE TABLE partsupp (
        ps_partkey INTEGER,
        ps_suppkey INTEGER,
        ps_availqty INTEGER,
        ps_supplycost NUMERIC,
        ps_comment VARCHAR,
);

CREATE TABLE customer (
        c_custkey INTEGER,
        c_name VARCHAR,
        c_address VARCHAR,
        c_nationkey INTEGER,
        c_phone VARCHAR,
        c_acctbal NUMERIC,
        c_mktsegment VARCHAR,
        c_comment VARCHAR,
);

CREATE TABLE orders (
        o_orderkey BIGINT,
        o_custkey INTEGER,
        o_orderstatus VARCHAR,
        o_totalprice NUMERIC,
        o_orderdate DATE,
        o_orderpriority VARCHAR,
        o_clerk VARCHAR,
        o_shippriority INTEGER,
        o_comment VARCHAR,
);

CREATE TABLE lineitem (
        l_orderkey BIGINT,
        l_partkey INTEGER,
        l_suppkey INTEGER,
        l_linenumber INTEGER,
        l_quantity NUMERIC,
        l_extendedprice NUMERIC,
        l_discount NUMERIC,
        l_tax NUMERIC,
        l_returnflag VARCHAR,
        l_linestatus VARCHAR,
        l_shipdate DATE,
        l_commitdate DATE,
        l_receiptdate DATE,
        l_shipinstruct VARCHAR,
        l_shipmode VARCHAR,
        l_comment VARCHAR,
);

CREATE TABLE nation (
        n_nationkey INTEGER,
        n_name VARCHAR,
        n_regionkey INTEGER,
        n_comment VARCHAR,
);

CREATE TABLE region (
        r_regionkey INTEGER,
        r_name VARCHAR,
        r_comment VARCHAR,
);

-- Q1
select
  l_returnflag,
  l_linestatus,
  sum(l_quantity) as sum_qty,
  sum(l_extendedprice) as sum_base_price,
  sum(l_extendedprice * (1 - l_discount)) as sum_disc_price,
  sum(l_extendedprice * (1 - l_discount) * (1 + l_tax)) as sum_charge,
  round(avg(l_quantity), 4) as avg_qty,
  round(avg(l_extendedprice), 4) as avg_price,
  round(avg(l_discount), 4) as avg_disc,
  count(*) as count_order
from
  lineitem
where
  l_shipdate <= date '1998-12-01' - interval '71' day
group by
  l_returnflag,
  l_linestatus
order by
  l_returnflag,
  l_linestatus;
