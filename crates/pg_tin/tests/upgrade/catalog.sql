-- The extension's catalog, for comparing an upgraded install with a fresh one.
SELECT p.oid::regprocedure::text AS function, pg_get_functiondef(p.oid) AS definition
  FROM pg_proc p JOIN pg_depend d ON d.objid = p.oid AND d.classid = 'pg_proc'::regclass
  JOIN pg_extension e ON e.oid = d.refobjid AND e.extname = 'pg_tin'
 WHERE d.deptype = 'e' ORDER BY 1;
SELECT o.oid::regoperator::text AS operator, o.oprcode, o.oprrest, o.oprjoin, obj_description(o.oid, 'pg_operator')
  FROM pg_operator o JOIN pg_depend d ON d.objid = o.oid AND d.classid = 'pg_operator'::regclass
  JOIN pg_extension e ON e.oid = d.refobjid AND e.extname = 'pg_tin'
 WHERE d.deptype = 'e' ORDER BY 1;
SELECT f.opfname, a.amopstrategy, a.amopopr::regoperator, a.amoppurpose, a.amopsortfamily::regclass IS NULL
  FROM pg_amop a JOIN pg_opfamily f ON f.oid = a.amopfamily
 WHERE f.opfname = 'text_tin_ops' ORDER BY 1, 2;
SELECT extversion FROM pg_extension WHERE extname = 'pg_tin';
