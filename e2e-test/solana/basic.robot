*** Settings ***
Library  RequestsLibrary
Library  OperatingSystem
Library  RPA.JSON
Library  DatabaseLibrary
Library  ../core-lib/request.py
Library  ../core-lib/pgconnection.py
Library  ../core-lib/example-reader.py

*** Variables ***
${CODE_COMPILER}  http://localhost:5000
${INDEX_MANAGER}  http://localhost:3000

*** Test Cases ***
#####################
# Test-solana-block #
#####################
Deploy test-solana-block, then check if data was inserted into DB
    # Configuration
    Connect To Database  psycopg2  graph-node  graph-node  let-me-in  localhost  5432

    # Remove table if exists
    Delete Table If Exists  sgd0.__diesel_schema_migrations
    Delete Table If Exists  sgd0.solana_block_ts

    # Compile request
    ${object} =  Read So Example  ../../user-example/solana/so/test-solana-block
    ${compile_res}=  Request.Post Request
    ...  ${CODE_COMPILER}/compile/so
    ...  ${object}
    Should be equal  ${compile_res["status"]}  success

    # Compile status
    Wait Until Keyword Succeeds
    ...  60x
    ...  3 sec
    ...  Pooling Status
    ...  ${compile_res["payload"]}

    # Deploy
    ${json}=  Convert String to JSON  {"compilation_id": "${compile_res["payload"]}"}
    ${deploy_res}=  Request.Post Request
    ...  ${CODE_COMPILER}/deploy/so
    ...  ${json}
    Should be equal  ${deploy_res["status"]}  success

    # Check that there is a table with data in it
    Wait Until Keyword Succeeds
    ...  10x
    ...  3 sec
    ...  Pooling Database Data
    ...  SELECT * FROM sgd0.solana_block_ts FETCH FIRST ROW ONLY


###########################
# Test-solana-transaction #
###########################
Deploy test-solana-transaction, then check if data was inserted into DB
    # Configuration
    Connect To Database  psycopg2  graph-node  graph-node  let-me-in  localhost  5432

    # Remove table if exists
    Delete Table If Exists  sgd0.__diesel_schema_migrations
    Delete Table If Exists  sgd0.solana_transaction_ts

    # Compile request
    ${object} =  Read So Example  ../../user-example/solana/so/test-solana-transaction
    ${compile_res}=  Request.Post Request
    ...  ${CODE_COMPILER}/compile/so
    ...  ${object}
    Should be equal  ${compile_res["status"]}  success

    # Compile status
    Wait Until Keyword Succeeds
    ...  60x
    ...  3 sec
    ...  Pooling Status
    ...  ${compile_res["payload"]}

    # Deploy
    ${json}=  Convert String to JSON  {"compilation_id": "${compile_res["payload"]}"}
    ${deploy_res}=  Request.Post Request
    ...  ${CODE_COMPILER}/deploy/so
    ...  ${json}
    Should be equal  ${deploy_res["status"]}  success

    # Check that there is a table with data in it
    Wait Until Keyword Succeeds
    ...  10x
    ...  3 sec
    ...  Pooling Database Data
    ...  SELECT * FROM sgd0.solana_transaction_ts FETCH FIRST ROW ONLY


############################
# Test-solana-log-messages #
############################
Deploy test-solana-log-messages, then check if data was inserted into DB
    # Configuration
    Connect To Database  psycopg2  graph-node  graph-node  let-me-in  localhost  5432

    # Remove table if exists
    Delete Table If Exists  sgd0.__diesel_schema_migrations
    Delete Table If Exists  sgd0.solana_log_messages_ts

    # Compile request
    ${object} =  Read So Example  ../../user-example/solana/so/test-solana-log-messages
    ${compile_res}=  Request.Post Request
    ...  ${CODE_COMPILER}/compile/so
    ...  ${object}
    Should be equal  ${compile_res["status"]}  success

    # Compile status
    Wait Until Keyword Succeeds
    ...  60x
    ...  3 sec
    ...  Pooling Status
    ...  ${compile_res["payload"]}

    # Deploy
    ${json}=  Convert String to JSON  {"compilation_id": "${compile_res["payload"]}"}
    ${deploy_res}=  Request.Post Request
    ...  ${CODE_COMPILER}/deploy/so
    ...  ${json}
    Should be equal  ${deploy_res["status"]}  success
    sleep  20 seconds  # Wait for indexing

    # Check that there is a table with data in it
    Wait Until Keyword Succeeds
    ...  10x
    ...  3 sec
    ...  Pooling Database Data
    ...  SELECT * FROM sgd0.solana_log_messages_ts FETCH FIRST ROW ONLY

###################
# Helper Function #
###################
*** Keywords ***
Pooling Status
    [Arguments]  ${payload}
    ${status_res} =    GET  ${CODE_COMPILER}/compile/status/${payload}  expected_status=200
    Should be equal   ${status_res.json()}[status]  success

Pooling Database Data
    [Arguments]  ${query}
    Check If Exists In Database  ${query}
